//! Finding audio files and reading what's in them, in parallel.
//!
//! Every file gets a quick fingerprint (size plus its first and last MiB, see
//! `hash::quick_hash`). When it matches what the database has for that path,
//! the tags known from last time are used instead of reading them again.
//! Everything learned is sent to the database's writer queue right away, so
//! an interrupted scan isn't wasted.

use std::collections::HashMap;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use rayon::iter::{ParallelBridge, ParallelIterator};
use tracing::{debug, info, warn};
use walkdir::WalkDir;

use crate::db::{Queue, TrackRow, WorkItem, Write};
use crate::fsops;
use crate::guard;
use crate::hash::{quick_hash, Cancelled, HASHED_BYTES};
use crate::model::FileInfo;
use crate::tags;
use crate::ui::{self, Lane};

#[derive(Debug, Clone)]
pub struct Scanned {
    pub abs: PathBuf,
    pub size: u64,
    pub mtime_ns: i64,
    /// Quick fingerprint (see `hash::quick_hash`).
    pub quick: String,
    /// Full content hash, if known (small files, or computed earlier).
    pub hash: Option<String>,
    pub info: FileInfo,
    /// The db row for this path, if there was one.
    pub row: Option<TrackRow>,
    /// Whether `info` came from the db instead of the file.
    pub from_cache: bool,
}

#[derive(Debug, Default)]
pub struct ScanResult {
    pub files: Vec<Scanned>,
    pub errors: Vec<(PathBuf, String)>,
    pub non_audio: Vec<PathBuf>,
    pub symlinks: usize,
    pub cancelled: bool,
}

/// Where a scan of the collection stores what it learns.
#[derive(Clone)]
pub struct Store {
    pub queue: Queue,
    /// The collection root (rows are stored relative to it).
    pub root: PathBuf,
}

impl Store {
    fn content(&self, s: &Scanned) {
        if let Ok(rel) = s.abs.strip_prefix(&self.root) {
            self.queue.send(Write::Content {
                path: rel.to_path_buf(),
                size: s.size,
                mtime_ns: s.mtime_ns,
                quick: s.quick.clone(),
                hash: s.hash.clone(),
                info: s.info.clone(),
            });
        }
    }
}

pub struct ScanOptions<'a> {
    /// Directories not to descend into.
    pub skip: &'a [PathBuf],
    /// Known rows, by absolute path. A row's tags are reused if the file's
    /// size and quick fingerprint still match it.
    pub cache: &'a HashMap<PathBuf, TrackRow>,
    /// Remove musort's own leftover working files (only inside the collection).
    pub clean_leftovers: bool,
    /// Store what's learned (scans of the collection; not of import sources).
    pub store: Option<Store>,
}

/// Live counters for the status bar.
#[derive(Default)]
struct Counters {
    found: AtomicUsize,
    unchanged: AtomicUsize,
    read: AtomicUsize,
    other: AtomicUsize,
}

pub fn scan(root: &Path, opts: &ScanOptions<'_>) -> ScanResult {
    info!("scanning {} ...", root.display());
    ui::show_lanes(&[Lane::Walk, Lane::Jobs]);
    let pb = ui::start(None, "scanning");
    let counters = Arc::new(Counters::default());
    {
        // (Not the bar's own position: this runs while the bar is drawing.)
        let (c, started, read0) = (counters.clone(), Instant::now(), HASHED_BYTES.load(Ordering::Relaxed));
        ui::headline(move || {
            let read = HASHED_BYTES.load(Ordering::Relaxed) - read0;
            let secs = started.elapsed().as_secs_f64().max(0.1);
            let mut s = format!(
                "{} audio file(s): {} unchanged since the last run, {} read",
                c.found.load(Ordering::Relaxed),
                c.unchanged.load(Ordering::Relaxed),
                c.read.load(Ordering::Relaxed)
            );
            if read > 0 {
                s.push_str(&format!(
                    "{}{} fingerprinted at {}/s",
                    ui::sym(" · ", ", "),
                    ui::size(read),
                    ui::size((read as f64 / secs) as u64)
                ));
            }
            let other = c.other.load(Ordering::Relaxed);
            if other > 0 {
                s.push_str(&format!("{}{other} other file(s)", ui::sym(" · ", ", ")));
            }
            s
        });
    }
    // Worker threads don't inherit the run's log span on their own.
    let span = tracing::Span::current();
    let out = Mutex::new(ScanResult::default());
    let walker = WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| !opts.skip.iter().any(|s| e.path() == s));

    walker.par_bridge().for_each(|entry| {
        let _span = span.enter();
        if guard::stop_requested() {
            return;
        }
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                let path = e.path().map(Path::to_path_buf).unwrap_or_default();
                warn!(path = %path.display(), "can't read: {e}");
                out.lock().unwrap().errors.push((path, e.to_string()));
                return;
            }
        };
        let path = entry.path();
        let ft = entry.file_type();
        let rel = path.strip_prefix(root).unwrap_or(path).display();
        if ft.is_dir() {
            let shown = rel.to_string();
            ui::lane(Lane::Walk, if shown.is_empty() { ".".to_string() } else { format!("{shown}/") });
            return;
        }
        if ft.is_symlink() {
            info!("  {rel}: symlink, skipped");
            out.lock().unwrap().symlinks += 1;
            return;
        }
        if fsops::is_tmp(path) {
            if opts.clean_leftovers {
                // Always safe to remove: a working copy only ever exists while
                // the original is still in place.
                match std::fs::remove_file(path) {
                    Ok(()) => warn!(path = %path.display(), "removed a leftover working file from an interrupted run"),
                    Err(e) => warn!(path = %path.display(), "can't remove leftover working file: {e}"),
                }
            }
            return;
        }
        if !tags::is_audio(path) {
            info!("  {rel}: not an audio file, leaving it alone");
            counters.other.fetch_add(1, Ordering::Relaxed);
            out.lock().unwrap().non_audio.push(path.to_path_buf());
            return;
        }
        match scan_file(path, &rel.to_string(), opts.cache, opts.store.as_ref()) {
            Ok(s) => {
                let c = if s.from_cache { &counters.unchanged } else { &counters.read };
                c.fetch_add(1, Ordering::Relaxed);
                counters.found.fetch_add(1, Ordering::Relaxed);
                pb.inc(1);
                out.lock().unwrap().files.push(s);
            }
            Err(e) if e.is::<Cancelled>() => {}
            Err(e) => {
                warn!(path = %path.display(), "skipping unreadable file: {e:#}");
                out.lock().unwrap().errors.push((path.to_path_buf(), format!("{e:#}")));
            }
        }
    });

    ui::finish();
    let mut out = out.into_inner().unwrap();
    out.cancelled = guard::stop_requested();
    out.files.sort_by(|a, b| a.abs.cmp(&b.abs));
    out
}

/// Fingerprint a file; reuse the cached tags if the fingerprint matches,
/// else read them. `rel` is the path as shown to the user.
fn scan_file(path: &Path, rel: &str, cache: &HashMap<PathBuf, TrackRow>, store: Option<&Store>) -> anyhow::Result<Scanned> {
    let md = std::fs::metadata(path)?;
    let size = md.len();
    let mtime_ns = md.mtime() * 1_000_000_000 + md.mtime_nsec();
    let row = cache.get(path).cloned();
    let job = ui::job(rel, size);
    job.stage("fingerprinting");
    let q = quick_hash(path, true)?;

    if let Some(r) = row.as_ref().filter(|r| r.size == size && r.quick.as_deref() == Some(q.quick.as_str())) {
        debug!("  {rel}: unchanged since the last run (same fingerprint), using what's known about it");
        let s = Scanned {
            abs: path.to_path_buf(),
            size,
            mtime_ns,
            quick: q.quick,
            hash: q.full.or_else(|| r.hash.clone()),
            info: r.info.clone(),
            row: row.clone(),
            from_cache: true,
        };
        if r.mtime_ns != mtime_ns || r.hash != s.hash {
            if let Some(store) = store {
                store.content(&s);
            }
        }
        return Ok(s);
    }

    job.stage("reading tags");
    let info = tags::read(path)?;
    drop(job);
    let secs = info.duration_ms / 1000;
    info!(
        "  {rel}: {}read {}, {}:{:02}, {:.1} MB: {}",
        if row.is_some() { "changed since the last run, re-" } else { "" },
        info.format,
        secs / 60,
        secs % 60,
        size as f64 / 1_000_000.0,
        info.meta.summary()
    );
    debug!(quick = q.quick, tags = %serde_json::to_string(&info.meta).unwrap_or_default(), "    details");
    let s = Scanned { abs: path.to_path_buf(), size, mtime_ns, quick: q.quick, hash: q.full, info, row, from_cache: false };
    if let Some(store) = store {
        store.content(&s);
    }
    Ok(s)
}

impl Scanned {
    pub fn work_item(&self) -> WorkItem {
        WorkItem {
            path: self.abs.clone(),
            size: self.size,
            mtime_ns: self.mtime_ns,
            quick: Some(self.quick.clone()),
            hash: self.hash.clone(),
            info: self.info.clone(),
            done: false,
        }
    }
}

#[derive(Debug, Default)]
pub struct Resumed {
    /// Every file of the interrupted run (done or not), for context such as
    /// which year an album has.
    pub all: Vec<Scanned>,
    /// The files still to do that are still there.
    pub todo: Vec<Scanned>,
    /// Files still to do that have disappeared since.
    pub gone: Vec<PathBuf>,
    pub errors: Vec<(PathBuf, String)>,
    /// Ctrl+C while checking: the lists are incomplete.
    pub cancelled: bool,
}

/// `--quick`: instead of walking directories, take a known list of files (an
/// interrupted run's, or the database's) and only check that each one not
/// yet done is still there and unchanged (size and mtime; no fingerprinting).
/// Changed files are read again. `root` is where paths are shown relative
/// to; `cache` the db rows by absolute path.
pub fn resume(items: Vec<WorkItem>, root: &Path, cache: &HashMap<PathBuf, TrackRow>, store: Option<&Store>) -> Resumed {
    let mut out = Resumed::default();
    let no_cache = HashMap::new();
    let todo = items.iter().filter(|i| !i.done).count();
    info!("checking {todo} known file(s) are still there and unchanged");
    let pb = ui::start(Some(todo as u64), "checking");
    let (gone, changed) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
    {
        let (gone, changed) = (gone.clone(), changed.clone());
        ui::headline(move || {
            let (g, c) = (gone.load(Ordering::Relaxed), changed.load(Ordering::Relaxed));
            if g + c == 0 {
                String::new()
            } else {
                format!("{g} gone, {c} changed (read again)")
            }
        });
    }
    for item in items {
        if guard::stop_requested() {
            break;
        }
        let rel = item.path.strip_prefix(root).unwrap_or(&item.path).display().to_string();
        if !item.done {
            ui::status(format!("{rel}: checking it's still there and unchanged"));
            pb.inc(1);
        }
        let mut stored = Scanned {
            abs: item.path.clone(),
            size: item.size,
            mtime_ns: item.mtime_ns,
            quick: item.quick.clone().unwrap_or_default(),
            hash: item.hash,
            info: item.info,
            row: cache.get(&item.path).cloned(),
            from_cache: true,
        };
        if item.done {
            out.all.push(stored);
            continue;
        }
        let Ok(md) = std::fs::symlink_metadata(&item.path) else {
            info!("  {rel}: gone, skipping it");
            gone.fetch_add(1, Ordering::Relaxed);
            out.gone.push(item.path);
            continue;
        };
        if md.len() == stored.size && md.mtime() * 1_000_000_000 + md.mtime_nsec() == stored.mtime_ns {
            debug!("  {rel}: still there and unchanged");
            if item.quick.is_none() {
                // Known from before fingerprints existed: fingerprint it once.
                ui::status(format!("{rel}: fingerprinting (first time)"));
                match quick_hash(&item.path, true) {
                    Ok(q) => {
                        stored.quick = q.quick;
                        stored.hash = q.full.or(stored.hash);
                        if let Some(store) = store {
                            store.content(&stored);
                        }
                    }
                    Err(e) => {
                        out.errors.push((item.path, format!("{e:#}")));
                        continue;
                    }
                }
            }
            out.all.push(stored.clone());
            out.todo.push(stored);
            continue;
        }
        changed.fetch_add(1, Ordering::Relaxed);
        ui::status(format!("{rel}: changed since, reading it again"));
        match scan_file(&item.path, &format!("{rel} (changed)"), &no_cache, store) {
            Ok(mut s) => {
                s.row = stored.row;
                out.all.push(s.clone());
                out.todo.push(s);
            }
            Err(e) => {
                warn!(path = %item.path.display(), "skipping unreadable file: {e:#}");
                out.errors.push((item.path, format!("{e:#}")));
            }
        }
    }
    ui::finish();
    out.cancelled = guard::stop_requested();
    out
}
