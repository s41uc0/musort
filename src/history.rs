//! The undo history: one append-only JSON-lines file per run, next to that
//! run's readable log, recording every change to a file (move, copy, tag
//! write) with what it was before.
//!
//! Each change is recorded and fsynced *before* it happens, then confirmed
//! with a `done` (or `failed`) record, so even after a crash the history
//! never misses a change. It's kept apart from the database because the
//! database is a disposable cache; this is not.

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::model::TrackMeta;
use crate::pipeline::Stats;

/// A path as stored in the history: relative to the collection root when
/// inside it (so the collection can be moved), absolute otherwise. Non-UTF-8
/// names are kept as raw bytes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum StoredPath {
    Utf8(String),
    Bytes { bytes: Vec<u8> },
}

impl StoredPath {
    fn new(root: &Path, p: &Path) -> Self {
        use std::os::unix::ffi::OsStrExt;
        let p = p.strip_prefix(root).unwrap_or(p);
        match p.to_str() {
            Some(s) => StoredPath::Utf8(s.to_string()),
            None => StoredPath::Bytes { bytes: p.as_os_str().as_bytes().to_vec() },
        }
    }

    /// Relative paths are inside the collection at `root`.
    pub fn resolve(&self, root: &Path) -> PathBuf {
        use std::os::unix::ffi::OsStrExt;
        let p = match self {
            StoredPath::Utf8(s) => PathBuf::from(s),
            StoredPath::Bytes { bytes } => PathBuf::from(std::ffi::OsStr::from_bytes(bytes)),
        };
        root.join(p)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Reason {
    /// Put in its place in the layout.
    Sorted,
    Unidentified,
    Duplicate,
    /// Put back by `musort undo`.
    Undo,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Start {
    pub run: String,
    pub command: String,
    pub argv: Vec<String>,
    pub version: String,
    /// RFC 3339.
    pub started: String,
    pub dry_run: bool,
    /// For `undo` runs: the run being undone.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub undoes: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
#[allow(clippy::large_enum_variant)] // a line of a file; boxing buys nothing
pub enum Record {
    Start(Start),
    /// A file moved or copied from `from` to `to`. `quick`: its quick
    /// fingerprint; `hash`: its full hash (only in histories from before
    /// quick fingerprints).
    Move {
        seq: u64,
        item: usize,
        copy: bool,
        from: StoredPath,
        to: StoredPath,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        quick: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        hash: Option<String>,
        reason: Reason,
        /// How it was identified, for display (`mb 97%`, `tags`, ...).
        #[serde(default)]
        note: String,
    },
    /// Tags of `file` changed from `before` to `after`.
    Tags { seq: u64, item: usize, file: StoredPath, before: TrackMeta, after: TrackMeta },
    /// `musort undo` of a copy-import: the copy at `file` was removed; the
    /// original at `original` was verified to still be intact first.
    Remove {
        seq: u64,
        item: usize,
        file: StoredPath,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        quick: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        hash: Option<String>,
        original: StoredPath,
    },
    /// The change `seq` happened. `quick`: the file's quick fingerprint
    /// afterwards (`hash`: its full hash, in older histories).
    Done {
        seq: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        quick: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        hash: Option<String>,
    },
    /// The change `seq` did not happen (the file was left as it was).
    Failed { seq: u64, error: String },
    Finish { finished: String, stats: Stats },
}

/// The history being written by the current run.
pub struct History {
    root: PathBuf,
    path: PathBuf,
    file: Mutex<File>,
    seq: AtomicU64,
}

impl History {
    pub fn create(root: &Path, path: &Path, start: Start) -> Result<Self> {
        let file = OpenOptions::new()
            .create_new(true)
            .append(true)
            .open(path)
            .with_context(|| format!("can't create the history file {}", path.display()))?;
        let h = History { root: root.to_path_buf(), path: path.to_path_buf(), file: Mutex::new(file), seq: AtomicU64::new(0) };
        h.append(&Record::Start(start))?;
        Ok(h)
    }

    fn append(&self, rec: &Record) -> Result<()> {
        let mut line = serde_json::to_string(rec)?;
        line.push('\n');
        let mut f = self.file.lock().unwrap_or_else(|e| e.into_inner());
        f.write_all(line.as_bytes())
            .and_then(|()| f.sync_data())
            .with_context(|| format!("can't write to the history file {}", self.path.display()))
    }

    fn next(&self) -> u64 {
        self.seq.fetch_add(1, Ordering::Relaxed) + 1
    }

    fn p(&self, p: &Path) -> StoredPath {
        StoredPath::new(&self.root, p)
    }

    /// Record a move/copy about to happen. Returns its sequence number.
    #[allow(clippy::too_many_arguments)]
    pub fn begin_move(
        &self,
        item: usize,
        copy: bool,
        from: &Path,
        to: &Path,
        quick: &str,
        reason: Reason,
        note: &str,
    ) -> Result<u64> {
        let seq = self.next();
        self.append(&Record::Move {
            seq,
            item,
            copy,
            from: self.p(from),
            to: self.p(to),
            quick: Some(quick.to_string()),
            hash: None,
            reason,
            note: note.to_string(),
        })?;
        Ok(seq)
    }

    pub fn begin_tags(&self, item: usize, file: &Path, before: &TrackMeta, after: &TrackMeta) -> Result<u64> {
        let seq = self.next();
        self.append(&Record::Tags { seq, item, file: self.p(file), before: before.clone(), after: after.clone() })?;
        Ok(seq)
    }

    pub fn begin_remove(&self, item: usize, file: &Path, quick: &str, original: &Path) -> Result<u64> {
        let seq = self.next();
        self.append(&Record::Remove {
            seq,
            item,
            file: self.p(file),
            quick: Some(quick.to_string()),
            hash: None,
            original: self.p(original),
        })?;
        Ok(seq)
    }

    /// `quick`: the file's quick fingerprint after the change, if it changed.
    pub fn done(&self, seq: u64, quick: Option<&str>) -> Result<()> {
        self.append(&Record::Done { seq, quick: quick.map(str::to_string), hash: None })
    }

    pub fn failed(&self, seq: u64, error: &anyhow::Error) -> Result<()> {
        self.append(&Record::Failed { seq, error: format!("{error:#}") })
    }

    pub fn finish(&self, stats: &Stats) -> Result<()> {
        self.append(&Record::Finish { finished: chrono::Local::now().to_rfc3339(), stats: stats.clone() })
    }
}

// ---- reading ----

/// Whether a recorded change happened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum State {
    /// `quick` (or, in older histories, `hash`): the file's content afterwards.
    Done { quick: Option<String>, hash: Option<String> },
    Failed(String),
    /// Neither confirmed nor failed: the run crashed right then. Whoever
    /// reads this has to look at the disk.
    Unknown,
}

#[derive(Debug, Clone)]
pub struct Change {
    pub record: Record,
    pub state: State,
}

impl Change {
    pub fn item(&self) -> usize {
        match &self.record {
            Record::Move { item, .. } | Record::Tags { item, .. } | Record::Remove { item, .. } => *item,
            _ => 0,
        }
    }
}

/// One run, read back from its history file.
#[derive(Debug, Clone)]
pub struct RunLog {
    pub start: Start,
    pub changes: Vec<Change>,
    pub finish: Option<(String, Stats)>,
    pub history_path: PathBuf,
}

impl RunLog {
    pub fn id(&self) -> &str {
        &self.start.run
    }

    pub fn log_path(&self) -> PathBuf {
        self.history_path.with_extension("log")
    }

    /// Changes grouped per file (in the order the files were handled).
    pub fn items(&self) -> Vec<Vec<&Change>> {
        let mut out: Vec<Vec<&Change>> = vec![];
        for c in &self.changes {
            match out.last_mut() {
                Some(group) if group[0].item() == c.item() => group.push(c),
                _ => out.push(vec![c]),
            }
        }
        out
    }

    /// Whether this run changed anything on disk.
    pub fn changed_anything(&self) -> bool {
        self.changes.iter().any(|c| !matches!(c.state, State::Failed(_)))
    }
}

pub fn load(path: &Path) -> Result<RunLog> {
    let f = File::open(path).with_context(|| format!("can't open {}", path.display()))?;
    let mut start = None;
    let mut changes: Vec<Change> = vec![];
    let mut finish = None;
    for (n, line) in BufReader::new(f).lines().enumerate() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        // A crash can leave a torn last line; anything else is corruption.
        let rec: Record = match serde_json::from_str(&line) {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!("{}:{}: unreadable history line skipped: {e}", path.display(), n + 1);
                continue;
            }
        };
        match rec {
            Record::Start(s) => start = Some(s),
            Record::Done { seq, quick, hash } => set_state(&mut changes, seq, State::Done { quick, hash }),
            Record::Failed { seq, error } => set_state(&mut changes, seq, State::Failed(error)),
            Record::Finish { finished, stats } => finish = Some((finished, stats)),
            rec => changes.push(Change { record: rec, state: State::Unknown }),
        }
    }
    let Some(start) = start else { bail!("{} is not a musort history file", path.display()) };
    Ok(RunLog { start, changes, finish, history_path: path.to_path_buf() })
}

fn set_state(changes: &mut [Change], seq: u64, state: State) {
    let found = changes.iter_mut().rev().find(|c| match &c.record {
        Record::Move { seq: s, .. } | Record::Tags { seq: s, .. } | Record::Remove { seq: s, .. } => *s == seq,
        _ => false,
    });
    if let Some(c) = found {
        c.state = state;
    }
}

/// Every run with a history in `logs_dir`, oldest first.
pub fn list(logs_dir: &Path) -> Result<Vec<RunLog>> {
    let mut out = vec![];
    let Ok(entries) = std::fs::read_dir(logs_dir) else { return Ok(out) };
    for e in entries {
        let path = e?.path();
        if path.extension().is_some_and(|x| x == "jsonl") {
            match load(&path) {
                Ok(r) => out.push(r),
                Err(e) => tracing::warn!("skipping {}: {e:#}", path.display()),
            }
        }
    }
    out.sort_by(|a, b| a.start.run.cmp(&b.start.run));
    Ok(out)
}

/// Find a run by id, unique id prefix, or `last` (the latest real run that
/// changed something).
pub fn find(runs: &[RunLog], query: &str) -> Result<usize> {
    if query == "last" {
        return runs
            .iter()
            .rposition(|r| !r.start.dry_run && r.changed_anything())
            .ok_or_else(|| anyhow::anyhow!("no run has changed anything yet"));
    }
    if let Some(i) = runs.iter().position(|r| r.id() == query) {
        return Ok(i);
    }
    let hits: Vec<usize> = (0..runs.len()).filter(|&i| runs[i].id().starts_with(query)).collect();
    match hits.as_slice() {
        [i] => Ok(*i),
        [] => bail!("no run {query:?} (see `musort log`)"),
        _ => bail!(
            "{query:?} matches {} runs: {}",
            hits.len(),
            hits.iter().map(|&i| runs[i].id()).collect::<Vec<_>>().join(", ")
        ),
    }
}

/// A new run id (its local start time, `2026-10-23_14-02-11`), unique in
/// `logs_dir`.
pub fn new_run_id(logs_dir: &Path) -> String {
    let base = chrono::Local::now().format("%Y-%m-%d_%H-%M-%S").to_string();
    let taken = |id: &str| {
        std::fs::read_dir(logs_dir)
            .map(|rd| rd.flatten().any(|e| e.file_name().to_string_lossy().starts_with(&format!("{id}_"))))
            .unwrap_or(false)
    };
    if !taken(&base) {
        return base;
    }
    (2..).map(|n| format!("{base}-{n}")).find(|id| !taken(id)).expect("unbounded")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn start(run: &str) -> Start {
        Start {
            run: run.into(),
            command: "import".into(),
            argv: vec![],
            version: "0".into(),
            started: String::new(),
            dry_run: false,
            undoes: None,
        }
    }

    #[test]
    fn write_and_read_back() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path();
        let path = root.join("r.jsonl");
        let h = History::create(root, &path, start("r1")).unwrap();
        let a = h.begin_move(1, false, Path::new("/src/a.mp3"), &root.join("A/a.mp3"), "h1", Reason::Sorted, "mb 97%").unwrap();
        h.done(a, None).unwrap();
        let t = h.begin_tags(1, &root.join("A/a.mp3"), &TrackMeta::default(), &TrackMeta::default()).unwrap();
        h.done(t, Some("h2")).unwrap();
        let b = h.begin_move(2, false, Path::new("/src/b.mp3"), &root.join("B/b.mp3"), "h3", Reason::Sorted, "").unwrap();
        h.failed(b, &anyhow::anyhow!("nope")).unwrap();
        h.begin_move(3, false, Path::new("/src/c.mp3"), &root.join("C/c.mp3"), "h4", Reason::Sorted, "").unwrap();
        h.finish(&Stats::default()).unwrap();
        // A torn line from a crash is skipped.
        std::fs::OpenOptions::new().append(true).open(&path).unwrap().write_all(b"{\"type\":\"do").unwrap();

        let r = load(&path).unwrap();
        assert_eq!(r.id(), "r1");
        assert_eq!(r.changes.len(), 4);
        assert_eq!(r.changes[0].state, State::Done { quick: None, hash: None });
        assert_eq!(r.changes[1].state, State::Done { quick: Some("h2".into()), hash: None });
        assert!(matches!(r.changes[2].state, State::Failed(_)));
        assert_eq!(r.changes[3].state, State::Unknown);
        assert_eq!(r.items().len(), 3);
        match &r.changes[0].record {
            Record::Move { from, to, .. } => {
                assert_eq!(from.resolve(root), PathBuf::from("/src/a.mp3"));
                assert_eq!(to, &StoredPath::Utf8("A/a.mp3".into()));
            }
            _ => panic!(),
        }
    }

    #[test]
    fn non_utf8_paths_survive() {
        use std::os::unix::ffi::OsStrExt;
        let root = Path::new("/c");
        let p = root.join(std::ffi::OsStr::from_bytes(b"caf\xe9.mp3"));
        let s = StoredPath::new(root, &p);
        let json = serde_json::to_string(&s).unwrap();
        let back: StoredPath = serde_json::from_str(&json).unwrap();
        assert_eq!(back.resolve(root), p);
    }

    #[test]
    fn find_runs() {
        let mk = |id: &str| RunLog { start: start(id), changes: vec![], finish: None, history_path: PathBuf::new() };
        let runs = vec![mk("2026-10-21_09-00-00"), mk("2026-10-22_09-00-00"), mk("2026-10-22_10-00-00")];
        assert_eq!(find(&runs, "2026-10-21").unwrap(), 0);
        assert!(find(&runs, "2026-10-22").is_err());
        assert_eq!(find(&runs, "2026-10-22_10").unwrap(), 2);
        assert!(find(&runs, "last").is_err()); // nothing changed anything
    }
}
