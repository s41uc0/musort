//! `.musort/database.db`: a cache of what musort knows about each file, plus
//! the journal of in-flight file operations. (The history of runs is in
//! `.musort/logs/`, see `history.rs`, because it must outlive this cache.)
//!
//! The database is *not* the source of truth: the files and their tags are.
//! Every run checks rows against the disk (by quick fingerprint, or by
//! size+mtime with `--quick`), so the db can be deleted at any time and is
//! rebuilt by the next `musort organize`.
//!
//! Cache writes (rows, fingerprints, `--quick` progress) go through a queue to
//! a writer thread with its own connection, which writes them in batches as
//! they come in: nobody waits for them, and what was learned before an
//! interruption is kept. The journal is written directly and synchronously.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};

use crate::model::FileInfo;

const SCHEMA_VERSION: i64 = 3;

fn tracks_table(name: &str) -> String {
    format!(
        "CREATE TABLE {name} (
            path          BLOB PRIMARY KEY,   -- relative to the collection root
            size          INTEGER NOT NULL,
            mtime_ns      INTEGER NOT NULL,
            quick         TEXT,               -- quick fingerprint (size, first and last MiB)
            hash          TEXT,               -- full content hash, when it was needed
            info          TEXT NOT NULL,      -- JSON FileInfo
            status        TEXT NOT NULL,      -- new | organized | unidentified
            identified_by TEXT,               -- musicbrainz | acoustid | tags
            lookup_failed_for TEXT,           -- quick fingerprint the last failed lookup was for
            first_seen    INTEGER NOT NULL,
            last_seen     INTEGER NOT NULL
        );"
    )
}

const JOURNAL_SCHEMA: &str = "
CREATE TABLE journal (
    id       INTEGER PRIMARY KEY,
    run_id   TEXT NOT NULL,
    op       TEXT NOT NULL,           -- move | copy
    src      BLOB NOT NULL,
    dst      BLOB NOT NULL,
    tmp      BLOB NOT NULL,
    hash     TEXT NOT NULL,           -- the source's full hash, or '' if not known
    state    TEXT NOT NULL,           -- started | placed | done | rolled-back | recovered
    started  INTEGER NOT NULL,
    updated  INTEGER NOT NULL
);
";

/// The files the last organize/import set out to handle, for `--quick`.
const WORKLIST_SCHEMA: &str = "
CREATE TABLE worklist_run (
    id       INTEGER PRIMARY KEY CHECK (id = 1),
    run_id   TEXT NOT NULL,
    command  TEXT NOT NULL,           -- organize | import
    source   BLOB,                    -- import: the source directory
    mode     TEXT NOT NULL            -- move | copy
);
CREATE TABLE worklist (
    path     BLOB PRIMARY KEY,        -- absolute
    size     INTEGER NOT NULL,
    mtime_ns INTEGER NOT NULL,
    quick    TEXT NOT NULL,
    hash     TEXT,
    info     TEXT NOT NULL,           -- JSON FileInfo
    done     INTEGER NOT NULL DEFAULT 0
);
";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// Scanned, not handled yet.
    New,
    Organized,
    Unidentified,
}

impl Status {
    fn as_str(self) -> &'static str {
        match self {
            Status::New => "new",
            Status::Organized => "organized",
            Status::Unidentified => "unidentified",
        }
    }
    fn parse(s: &str) -> Status {
        match s {
            "organized" => Status::Organized,
            "new" => Status::New,
            _ => Status::Unidentified,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct TrackRow {
    pub path: PathBuf,
    pub size: u64,
    pub mtime_ns: i64,
    pub quick: Option<String>,
    pub hash: Option<String>,
    pub info: FileInfo,
    pub status: Status,
    pub identified_by: Option<String>,
    pub lookup_failed_for: Option<String>,
}

#[derive(Debug, Clone)]
pub struct JournalEntry {
    pub id: i64,
    pub op: String,
    pub src: PathBuf,
    pub dst: PathBuf,
    pub tmp: PathBuf,
    /// Empty if the source's full hash wasn't known.
    pub hash: String,
    pub state: String,
}

/// What the interrupted run was doing (see `WORKLIST_SCHEMA`).
#[derive(Debug, Clone)]
pub struct WorkRun {
    pub run_id: String,
    pub command: String,
    pub source: Option<PathBuf>,
    pub copy: bool,
}

/// One file of the worklist, as it was scanned.
#[derive(Debug, Clone)]
pub struct WorkItem {
    pub path: PathBuf,
    pub size: u64,
    pub mtime_ns: i64,
    pub quick: Option<String>,
    pub hash: Option<String>,
    pub info: FileInfo,
    pub done: bool,
}

/// A cache write, for the writer thread. Track paths are relative to the
/// collection root; worklist paths are absolute.
#[derive(Debug)]
#[allow(clippy::large_enum_variant)] // short-lived messages; boxing buys nothing
pub enum Write {
    /// What a scan saw of a file. Unknown files are added as `new`; known
    /// ones keep their status. A different quick fingerprint (or size)
    /// drops the old full hash, since that's for other content.
    Content { path: PathBuf, size: u64, mtime_ns: i64, quick: String, hash: Option<String>, info: FileInfo },
    /// A full hash computed later, valid while the quick fingerprint matches.
    FullHash { path: PathBuf, quick: String, hash: String },
    /// A file handled by the pipeline; `old`: where it was before, if it moved.
    Track { old: Option<PathBuf>, row: TrackRow },
    Delete(PathBuf),
    WorklistDone(PathBuf),
}

#[allow(clippy::large_enum_variant)]
enum Msg {
    Write(Write),
    Flush(mpsc::Sender<()>),
    Stop,
}

/// Where cache writes go. Cheap to clone, and usable from any thread.
#[derive(Clone)]
pub struct Queue(mpsc::Sender<Msg>);

impl Queue {
    pub fn send(&self, w: Write) {
        // If the writer is gone the run is ending anyway; it's only a cache.
        let _ = self.0.send(Msg::Write(w));
    }
}

pub struct Db {
    conn: Connection,
    queue: Queue,
    writer: Option<JoinHandle<()>>,
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn pb(p: &Path) -> &[u8] {
    p.as_os_str().as_bytes()
}

fn bp(b: Vec<u8>) -> PathBuf {
    PathBuf::from(OsStr::from_bytes(&b))
}

fn connect(path: &Path, synchronous: &str) -> Result<Connection> {
    let conn = Connection::open(path).with_context(|| format!("can't open database {}", path.display()))?;
    conn.busy_timeout(Duration::from_secs(30))?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", synchronous)?;
    Ok(conn)
}

impl Db {
    pub fn open(path: &Path) -> Result<Self> {
        let conn = connect(path, "FULL")?;
        let version: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
        match version {
            0 => conn.execute_batch(&format!(
                "BEGIN; {} {JOURNAL_SCHEMA} {WORKLIST_SCHEMA} PRAGMA user_version = {SCHEMA_VERSION}; COMMIT;",
                tracks_table("tracks")
            ))?,
            // v1/v2 -> v3: hashes become optional, quick fingerprints are added.
            // Known full hashes are kept; the worklist is only transient.
            1 | 2 => conn.execute_batch(&format!(
                "BEGIN;
                 {}
                 INSERT INTO tracks_v3 (path, size, mtime_ns, quick, hash, info, status, identified_by,
                                        lookup_failed_for, first_seen, last_seen)
                     SELECT path, size, mtime_ns, NULL, hash, info, status, identified_by,
                            lookup_failed_for, first_seen, last_seen FROM tracks;
                 DROP TABLE tracks;
                 ALTER TABLE tracks_v3 RENAME TO tracks;
                 DROP TABLE IF EXISTS runs;
                 DROP TABLE IF EXISTS worklist;
                 DROP TABLE IF EXISTS worklist_run;
                 {WORKLIST_SCHEMA}
                 PRAGMA user_version = {SCHEMA_VERSION};
                 COMMIT;",
                tracks_table("tracks_v3")
            ))?,
            SCHEMA_VERSION => {}
            v => anyhow::bail!(
                "{} was made by a newer musort (schema {v}); upgrade musort, or delete the file \
                 (it is only a cache and will be rebuilt)",
                path.display()
            ),
        }
        // Cache writes don't need to survive a power cut (a lost row only
        // means a file is read again), so the writer doesn't wait for fsync.
        let wconn = connect(path, "NORMAL")?;
        let (tx, rx) = mpsc::channel();
        let writer = std::thread::Builder::new().name("db-writer".into()).spawn(move || writer(wconn, rx))?;
        Ok(Db { conn, queue: Queue(tx), writer: Some(writer) })
    }

    /// A handle for cache writes (see [`Write`]).
    pub fn queue(&self) -> Queue {
        self.queue.clone()
    }

    /// Wait until every cache write queued so far is in the database.
    pub fn flush(&self) {
        let (tx, rx) = mpsc::channel();
        if self.queue.0.send(Msg::Flush(tx)).is_ok() {
            let _ = rx.recv();
        }
    }

    fn row_to_track(r: &rusqlite::Row<'_>) -> rusqlite::Result<TrackRow> {
        let info: String = r.get("info")?;
        Ok(TrackRow {
            path: bp(r.get("path")?),
            size: r.get::<_, i64>("size")? as u64,
            mtime_ns: r.get("mtime_ns")?,
            quick: r.get("quick")?,
            hash: r.get("hash")?,
            info: serde_json::from_str(&info).unwrap_or_default(),
            status: Status::parse(&r.get::<_, String>("status")?),
            identified_by: r.get("identified_by")?,
            lookup_failed_for: r.get("lookup_failed_for")?,
        })
    }

    pub fn all_tracks(&self) -> Result<HashMap<PathBuf, TrackRow>> {
        self.flush();
        let mut st = self.conn.prepare("SELECT * FROM tracks")?;
        let rows = st.query_map([], Self::row_to_track)?;
        let mut out = HashMap::new();
        for r in rows {
            let r = r?;
            out.insert(r.path.clone(), r);
        }
        Ok(out)
    }

    pub fn put_track(&self, old: Option<PathBuf>, row: TrackRow) {
        self.queue.send(Write::Track { old, row });
    }

    pub fn delete_track(&self, path: &Path) {
        self.queue.send(Write::Delete(path.to_path_buf()));
    }

    // ---- worklist ----

    /// Replace the worklist with `items`, all still to do.
    pub fn worklist_save(&self, run: &WorkRun, items: &[WorkItem]) -> Result<()> {
        self.flush();
        let tx = self.conn.unchecked_transaction()?;
        tx.execute("DELETE FROM worklist_run", [])?;
        tx.execute("DELETE FROM worklist", [])?;
        tx.execute(
            "INSERT INTO worklist_run (id, run_id, command, source, mode) VALUES (1, ?1, ?2, ?3, ?4)",
            params![run.run_id, run.command, run.source.as_deref().map(pb), if run.copy { "copy" } else { "move" }],
        )?;
        {
            let mut st = tx.prepare(
                "INSERT INTO worklist (path, size, mtime_ns, quick, hash, info) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            )?;
            for i in items {
                st.execute(params![
                    pb(&i.path),
                    i.size as i64,
                    i.mtime_ns,
                    i.quick.as_deref().unwrap_or(""),
                    i.hash,
                    serde_json::to_string(&i.info)?
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub fn worklist_done(&self, path: &Path) {
        self.queue.send(Write::WorklistDone(path.to_path_buf()));
    }

    pub fn worklist_clear(&self) -> Result<()> {
        self.flush();
        let tx = self.conn.unchecked_transaction()?;
        tx.execute("DELETE FROM worklist_run", [])?;
        tx.execute("DELETE FROM worklist", [])?;
        tx.commit()?;
        Ok(())
    }

    pub fn worklist_load(&self) -> Result<Option<(WorkRun, Vec<WorkItem>)>> {
        self.flush();
        let run = self
            .conn
            .query_row("SELECT run_id, command, source, mode FROM worklist_run", [], |r| {
                Ok(WorkRun {
                    run_id: r.get(0)?,
                    command: r.get(1)?,
                    source: r.get::<_, Option<Vec<u8>>>(2)?.map(bp),
                    copy: r.get::<_, String>(3)? == "copy",
                })
            })
            .optional()?;
        let Some(run) = run else { return Ok(None) };
        let mut st =
            self.conn.prepare("SELECT path, size, mtime_ns, quick, hash, info, done FROM worklist ORDER BY rowid")?;
        let items = st
            .query_map([], |r| {
                let info: String = r.get(5)?;
                Ok(WorkItem {
                    path: bp(r.get(0)?),
                    size: r.get::<_, i64>(1)? as u64,
                    mtime_ns: r.get(2)?,
                    quick: r.get::<_, String>(3).ok().filter(|q| !q.is_empty()),
                    hash: r.get(4)?,
                    info: serde_json::from_str(&info).unwrap_or_default(),
                    done: r.get(6)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(Some((run, items)))
    }

    // ---- journal ----

    pub fn journal_begin(&self, run_id: &str, op: &str, src: &Path, dst: &Path, tmp: &Path, hash: &str) -> Result<i64> {
        let t = now();
        self.conn.execute(
            "INSERT INTO journal (run_id, op, src, dst, tmp, hash, state, started, updated)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'started', ?7, ?7)",
            params![run_id, op, pb(src), pb(dst), pb(tmp), hash, t],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn journal_set_state(&self, id: i64, state: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE journal SET state = ?2, updated = ?3 WHERE id = ?1",
            params![id, state, now()],
        )?;
        Ok(())
    }

    pub fn journal_pending(&self) -> Result<Vec<JournalEntry>> {
        let mut st = self.conn.prepare(
            "SELECT id, op, src, dst, tmp, hash, state FROM journal
             WHERE state IN ('started', 'placed') ORDER BY id",
        )?;
        let rows = st.query_map([], |r| {
            Ok(JournalEntry {
                id: r.get(0)?,
                op: r.get(1)?,
                src: bp(r.get(2)?),
                dst: bp(r.get(3)?),
                tmp: bp(r.get(4)?),
                hash: r.get(5)?,
                state: r.get(6)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }
}

impl Drop for Db {
    fn drop(&mut self) {
        // Write out whatever is still queued, then stop the writer.
        let _ = self.queue.0.send(Msg::Stop);
        if let Some(w) = self.writer.take() {
            let _ = w.join();
        }
    }
}

/// The writer thread: takes queued writes and applies them in batches (one
/// transaction per batch, at most ~200ms after the first write of a batch).
fn writer(conn: Connection, rx: mpsc::Receiver<Msg>) {
    loop {
        let Ok(first) = rx.recv() else { return };
        let mut writes = vec![];
        let mut acks = vec![];
        let mut stop = false;
        let mut take = |m: Msg, writes: &mut Vec<Write>| match m {
            Msg::Write(w) => {
                writes.push(w);
                true
            }
            Msg::Flush(ack) => {
                acks.push(ack);
                false
            }
            Msg::Stop => {
                stop = true;
                false
            }
        };
        if take(first, &mut writes) {
            let deadline = Instant::now() + Duration::from_millis(200);
            while writes.len() < 2000 {
                match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                    Ok(m) => {
                        if !take(m, &mut writes) {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        }
        if !writes.is_empty() {
            if let Err(e) = apply(&conn, &writes) {
                tracing::warn!("couldn't update the database cache ({} change(s) lost; files will be read again): {e:#}", writes.len());
            }
        }
        for a in acks {
            let _ = a.send(());
        }
        if stop {
            // Anything sent after Stop (there shouldn't be) is dropped.
            return;
        }
    }
}

fn apply(conn: &Connection, writes: &[Write]) -> Result<()> {
    let tx = conn.unchecked_transaction()?;
    let t = now();
    for w in writes {
        match w {
            Write::Content { path, size, mtime_ns, quick, hash, info } => {
                tx.execute(
                    "INSERT INTO tracks (path, size, mtime_ns, quick, hash, info, status, first_seen, last_seen)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'new', ?7, ?7)
                     ON CONFLICT(path) DO UPDATE SET
                        hash = CASE WHEN tracks.quick IS ?4 AND tracks.size = ?2
                                    THEN COALESCE(?5, tracks.hash) ELSE ?5 END,
                        size = ?2, mtime_ns = ?3, quick = ?4, info = ?6, last_seen = ?7",
                    params![pb(path), *size as i64, mtime_ns, quick, hash, serde_json::to_string(info)?, t],
                )?;
            }
            Write::FullHash { path, quick, hash } => {
                tx.execute("UPDATE tracks SET hash = ?3 WHERE path = ?1 AND quick IS ?2", params![pb(path), quick, hash])?;
            }
            Write::Track { old, row } => {
                if let Some(old) = old.as_deref().filter(|o| *o != row.path) {
                    tx.execute("DELETE FROM tracks WHERE path = ?1", [pb(old)])?;
                }
                tx.execute(
                    "INSERT INTO tracks (path, size, mtime_ns, quick, hash, info, status, identified_by,
                                         lookup_failed_for, first_seen, last_seen)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?10)
                     ON CONFLICT(path) DO UPDATE SET size=?2, mtime_ns=?3, quick=?4, hash=?5, info=?6, status=?7,
                        identified_by=?8, lookup_failed_for=?9, last_seen=?10",
                    params![
                        pb(&row.path),
                        row.size as i64,
                        row.mtime_ns,
                        row.quick,
                        row.hash,
                        serde_json::to_string(&row.info)?,
                        row.status.as_str(),
                        row.identified_by,
                        row.lookup_failed_for,
                        t
                    ],
                )?;
            }
            Write::Delete(path) => {
                tx.execute("DELETE FROM tracks WHERE path = ?1", [pb(path)])?;
            }
            Write::WorklistDone(path) => {
                tx.execute("UPDATE worklist SET done = 1 WHERE path = ?1", [pb(path)])?;
            }
        }
    }
    tx.commit()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(path: &str, quick: &str, hash: Option<&str>) -> TrackRow {
        TrackRow {
            path: path.into(),
            size: 10,
            mtime_ns: 1,
            quick: Some(quick.into()),
            hash: hash.map(str::to_string),
            info: FileInfo::default(),
            status: Status::Organized,
            identified_by: None,
            lookup_failed_for: None,
        }
    }

    #[test]
    fn queued_writes_land_and_content_keeps_matching_hash() {
        let d = tempfile::tempdir().unwrap();
        let db = Db::open(&d.path().join("db")).unwrap();
        db.put_track(None, row("a", "q1", Some("h1")));
        // Same content seen again by a scan: the full hash stays.
        db.queue().send(Write::Content {
            path: "a".into(),
            size: 10,
            mtime_ns: 2,
            quick: "q1".into(),
            hash: None,
            info: FileInfo::default(),
        });
        // A new file shows up as `new`.
        db.queue().send(Write::Content {
            path: "b".into(),
            size: 5,
            mtime_ns: 1,
            quick: "q2".into(),
            hash: None,
            info: FileInfo::default(),
        });
        let rows = db.all_tracks().unwrap();
        let a = &rows[Path::new("a")];
        assert_eq!((a.hash.as_deref(), a.mtime_ns, a.status), (Some("h1"), 2, Status::Organized));
        assert_eq!(rows[Path::new("b")].status, Status::New);

        // Different content: the old full hash goes.
        db.queue().send(Write::Content {
            path: "a".into(),
            size: 10,
            mtime_ns: 3,
            quick: "q9".into(),
            hash: None,
            info: FileInfo::default(),
        });
        db.delete_track(Path::new("b"));
        let rows = db.all_tracks().unwrap();
        assert_eq!(rows[Path::new("a")].hash, None);
        assert!(!rows.contains_key(Path::new("b")));
    }

    #[test]
    fn migrates_v2_keeping_hashes() {
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("db");
        {
            let c = Connection::open(&path).unwrap();
            c.execute_batch(
                "CREATE TABLE tracks (path BLOB PRIMARY KEY, size INTEGER NOT NULL, mtime_ns INTEGER NOT NULL,
                    hash TEXT NOT NULL, info TEXT NOT NULL, status TEXT NOT NULL, identified_by TEXT,
                    lookup_failed_for TEXT, first_seen INTEGER NOT NULL, last_seen INTEGER NOT NULL);
                 CREATE TABLE journal (id INTEGER PRIMARY KEY, run_id TEXT NOT NULL, op TEXT NOT NULL,
                    src BLOB NOT NULL, dst BLOB NOT NULL, tmp BLOB NOT NULL, hash TEXT NOT NULL,
                    state TEXT NOT NULL, started INTEGER NOT NULL, updated INTEGER NOT NULL);
                 CREATE TABLE worklist_run (id INTEGER PRIMARY KEY);
                 CREATE TABLE worklist (path BLOB PRIMARY KEY);
                 INSERT INTO tracks VALUES (x'61', 10, 1, 'oldhash', '{}', 'organized', 'tags', NULL, 0, 0);
                 PRAGMA user_version = 2;",
            )
            .unwrap();
        }
        let db = Db::open(&path).unwrap();
        let rows = db.all_tracks().unwrap();
        let a = &rows[Path::new("a")];
        assert_eq!((a.hash.as_deref(), a.quick.as_deref(), a.status), (Some("oldhash"), None, Status::Organized));
        assert!(db.worklist_load().unwrap().is_none());
    }
}
