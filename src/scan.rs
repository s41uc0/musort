//! Finding audio files and reading what's in them, in parallel.

use std::collections::HashMap;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use rayon::iter::{ParallelBridge, ParallelIterator};
use tracing::{debug, info, warn};
use walkdir::WalkDir;

use crate::db::TrackRow;
use crate::fsops;
use crate::guard;
use crate::hash::{hash_file, Cancelled};
use crate::model::FileInfo;
use crate::tags;
use crate::ui;

#[derive(Debug, Clone)]
pub struct Scanned {
    pub abs: PathBuf,
    pub hash: String,
    pub info: FileInfo,
    /// The db row for this path, if there was one.
    pub row: Option<TrackRow>,
    /// Whether `info`/`hash` came from the db instead of the file.
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

pub struct ScanOptions<'a> {
    /// Directories not to descend into.
    pub skip: &'a [PathBuf],
    /// Known rows, by absolute path. A row is trusted only if size and mtime
    /// still match the file exactly.
    pub cache: &'a HashMap<PathBuf, TrackRow>,
    /// Remove musort's own leftover working files (only inside the collection).
    pub clean_leftovers: bool,
}

pub fn scan(root: &Path, opts: &ScanOptions<'_>) -> ScanResult {
    info!("scanning {} ...", root.display());
    let pb = ui::start(None, "scanning");
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
        if ft.is_dir() {
            return;
        }
        let rel = path.strip_prefix(root).unwrap_or(path).display();
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
            out.lock().unwrap().non_audio.push(path.to_path_buf());
            return;
        }
        pb.set_message(path.file_name().unwrap_or_default().to_string_lossy().into_owned());
        match scan_file(path, &rel.to_string(), opts.cache) {
            Ok(s) => {
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

/// `rel` is the path as shown to the user (relative to the scan root).
fn scan_file(path: &Path, rel: &str, cache: &HashMap<PathBuf, TrackRow>) -> anyhow::Result<Scanned> {
    let md = std::fs::metadata(path)?;
    let size = md.len();
    let mtime_ns = md.mtime() * 1_000_000_000 + md.mtime_nsec();
    let row = cache.get(path).cloned();
    if let Some(r) = &row {
        if r.size == size && r.mtime_ns == mtime_ns {
            info!("  {rel}: unchanged since the last run, using what's known about it");
            return Ok(Scanned {
                abs: path.to_path_buf(),
                hash: r.hash.clone(),
                info: r.info.clone(),
                row,
                from_cache: true,
            });
        }
    }
    let info = tags::read(path)?;
    let hash = hash_file(path, true)?;
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
    debug!(hash, tags = %serde_json::to_string(&info.meta).unwrap_or_default(), "    details");
    Ok(Scanned { abs: path.to_path_buf(), hash, info, row, from_cache: false })
}
