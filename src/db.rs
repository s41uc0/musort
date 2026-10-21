//! `.musort/database.db`: a cache of what musort knows about each file, plus
//! the journal of in-flight file operations and a history of runs.
//!
//! The database is *not* the source of truth: the files and their tags are.
//! Every run checks rows against the disk (by size+mtime, then by content
//! hash), so the db can be deleted at any time and is rebuilt by the next
//! `musort organize`.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use rusqlite::{params, Connection};

use crate::model::FileInfo;

const SCHEMA_VERSION: i64 = 1;

const SCHEMA: &str = "
CREATE TABLE tracks (
    path          BLOB PRIMARY KEY,   -- relative to the collection root
    size          INTEGER NOT NULL,
    mtime_ns      INTEGER NOT NULL,
    hash          TEXT NOT NULL,
    info          TEXT NOT NULL,      -- JSON FileInfo
    status        TEXT NOT NULL,      -- organized | unidentified
    identified_by TEXT,               -- musicbrainz | acoustid | tags
    lookup_failed_for TEXT,           -- hash the last failed lookup was done for
    first_seen    INTEGER NOT NULL,
    last_seen     INTEGER NOT NULL
);
CREATE INDEX tracks_hash ON tracks(hash);

CREATE TABLE journal (
    id       INTEGER PRIMARY KEY,
    run_id   TEXT NOT NULL,
    op       TEXT NOT NULL,           -- move | copy
    src      BLOB NOT NULL,
    dst      BLOB NOT NULL,
    tmp      BLOB NOT NULL,
    hash     TEXT NOT NULL,
    state    TEXT NOT NULL,           -- started | placed | done | rolled-back | recovered
    started  INTEGER NOT NULL,
    updated  INTEGER NOT NULL
);

CREATE TABLE runs (
    id          TEXT PRIMARY KEY,
    command     TEXT NOT NULL,
    started     INTEGER NOT NULL,
    finished    INTEGER,
    interrupted INTEGER NOT NULL DEFAULT 0,
    summary     TEXT
);
";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Organized,
    Unidentified,
}

impl Status {
    fn as_str(self) -> &'static str {
        match self {
            Status::Organized => "organized",
            Status::Unidentified => "unidentified",
        }
    }
    fn parse(s: &str) -> Status {
        match s {
            "organized" => Status::Organized,
            _ => Status::Unidentified,
        }
    }
}

#[derive(Debug, Clone)]
pub struct TrackRow {
    pub path: PathBuf,
    pub size: u64,
    pub mtime_ns: i64,
    pub hash: String,
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
    pub hash: String,
    pub state: String,
}

#[derive(Debug, Clone)]
pub struct RunInfo {
    pub command: String,
    pub started: i64,
    pub finished: Option<i64>,
    pub interrupted: bool,
}

pub struct Db {
    conn: Connection,
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

impl Db {
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path).with_context(|| format!("can't open database {}", path.display()))?;
        conn.busy_timeout(std::time::Duration::from_secs(10))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "FULL")?;
        let version: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
        match version {
            0 => {
                conn.execute_batch(SCHEMA)?;
                conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
            }
            SCHEMA_VERSION => {}
            v => anyhow::bail!(
                "{} was made by a newer musort (schema {v}); upgrade musort, or delete the file \
                 (it is only a cache and will be rebuilt)",
                path.display()
            ),
        }
        Ok(Db { conn })
    }

    fn row_to_track(r: &rusqlite::Row<'_>) -> rusqlite::Result<TrackRow> {
        let info: String = r.get("info")?;
        Ok(TrackRow {
            path: bp(r.get("path")?),
            size: r.get::<_, i64>("size")? as u64,
            mtime_ns: r.get("mtime_ns")?,
            hash: r.get("hash")?,
            info: serde_json::from_str(&info).unwrap_or_default(),
            status: Status::parse(&r.get::<_, String>("status")?),
            identified_by: r.get("identified_by")?,
            lookup_failed_for: r.get("lookup_failed_for")?,
        })
    }

    pub fn all_tracks(&self) -> Result<HashMap<PathBuf, TrackRow>> {
        let mut st = self.conn.prepare("SELECT * FROM tracks")?;
        let rows = st.query_map([], Self::row_to_track)?;
        let mut out = HashMap::new();
        for r in rows {
            let r = r?;
            out.insert(r.path.clone(), r);
        }
        Ok(out)
    }

    /// Insert or replace the row for `row.path`, removing the row for
    /// `old_path` (if the file moved) in the same transaction.
    pub fn put_track(&self, old_path: Option<&Path>, row: &TrackRow) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        if let Some(old) = old_path {
            tx.execute("DELETE FROM tracks WHERE path = ?1", [pb(old)])?;
        }
        let t = now();
        tx.execute(
            "INSERT INTO tracks (path, size, mtime_ns, hash, info, status, identified_by, lookup_failed_for, first_seen, last_seen)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?9)
             ON CONFLICT(path) DO UPDATE SET size=?2, mtime_ns=?3, hash=?4, info=?5, status=?6,
                identified_by=?7, lookup_failed_for=?8, last_seen=?9",
            params![
                pb(&row.path),
                row.size as i64,
                row.mtime_ns,
                row.hash,
                serde_json::to_string(&row.info)?,
                row.status.as_str(),
                row.identified_by,
                row.lookup_failed_for,
                t
            ],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn delete_track(&self, path: &Path) -> Result<()> {
        self.conn.execute("DELETE FROM tracks WHERE path = ?1", [pb(path)])?;
        Ok(())
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

    // ---- runs ----

    pub fn run_start(&self, id: &str, command: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO runs (id, command, started) VALUES (?1, ?2, ?3)",
            params![id, command, now()],
        )?;
        Ok(())
    }

    pub fn run_finish(&self, id: &str, interrupted: bool, summary: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE runs SET finished = ?2, interrupted = ?3, summary = ?4 WHERE id = ?1",
            params![id, now(), interrupted, summary],
        )?;
        Ok(())
    }

    pub fn recent_runs(&self, limit: usize) -> Result<Vec<RunInfo>> {
        let mut st = self.conn.prepare(
            "SELECT command, started, finished, interrupted FROM runs ORDER BY started DESC, rowid DESC LIMIT ?1",
        )?;
        let rows = st.query_map([limit as i64], |r| {
            Ok(RunInfo { command: r.get(0)?, started: r.get(1)?, finished: r.get(2)?, interrupted: r.get(3)? })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }
}
