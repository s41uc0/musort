//! The only code that moves, copies or deletes files.
//!
//! Rules:
//! * never overwrite an existing file (`RENAME_NOREPLACE` / hard links);
//! * a copy is fsynced and its hash verified before it takes the destination
//!   name, and the source is removed only after that;
//! * every multi-step operation is journaled in the db, so an operation cut
//!   short by a crash or `kill -9` is finished or rolled back on the next run.

use std::fs::{self, File};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use rustix::fs::{renameat_with, RenameFlags, CWD};
use rustix::io::Errno;
use tracing::{info, warn};

use crate::db::Db;
use crate::hash::hash_file;

const TMP_SUFFIX: &str = ".musort-tmp";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Move,
    Copy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    Rename,
    Copy,
}

impl std::fmt::Display for Method {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Method::Rename => "rename",
            Method::Copy => "verified copy",
        })
    }
}

/// The hidden working file musort uses next to `path`.
pub fn tmp_path_for(path: &Path) -> PathBuf {
    let name = path.file_name().map(|n| n.to_os_string()).unwrap_or_default();
    let mut tmp = std::ffi::OsString::from(".");
    tmp.push(name);
    tmp.push(TMP_SUFFIX);
    path.with_file_name(tmp)
}

pub fn is_tmp(path: &Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.starts_with('.') && n.ends_with(TMP_SUFFIX))
}

pub fn fsync_file(path: &Path) -> Result<()> {
    File::open(path)?.sync_all()?;
    Ok(())
}

pub fn fsync_dir(dir: &Path) -> Result<()> {
    File::open(dir)?.sync_all()?;
    Ok(())
}

pub fn fsync_parent(path: &Path) -> Result<()> {
    match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => fsync_dir(p),
        _ => Ok(()),
    }
}

/// Rename that fails instead of replacing an existing `dst`.
/// Returns `Err(CrossDevice)` when `src` and `dst` are on different filesystems.
pub fn rename_noreplace(src: &Path, dst: &Path) -> std::result::Result<(), RenameError> {
    match renameat_with(CWD, src, CWD, dst, RenameFlags::NOREPLACE) {
        Ok(()) => Ok(()),
        Err(Errno::EXIST) => Err(RenameError::Exists),
        Err(Errno::XDEV) => Err(RenameError::CrossDevice),
        // Filesystem doesn't support RENAME_NOREPLACE: hard link + unlink is
        // just as safe (link fails if dst exists).
        Err(Errno::INVAL) | Err(Errno::NOSYS) | Err(Errno::OPNOTSUPP) => match fs::hard_link(src, dst) {
            Ok(()) => fs::remove_file(src).map_err(RenameError::Io),
            Err(e) if e.kind() == ErrorKind::AlreadyExists => Err(RenameError::Exists),
            Err(e) if e.raw_os_error() == Some(Errno::XDEV.raw_os_error()) => Err(RenameError::CrossDevice),
            Err(_) => {
                // No hard links either (FAT, some network shares). Check-then-
                // rename leaves a tiny race, but musort holds the collection lock.
                if dst.symlink_metadata().is_ok() {
                    return Err(RenameError::Exists);
                }
                fs::rename(src, dst).map_err(RenameError::Io)
            }
        },
        Err(e) => Err(RenameError::Io(e.into())),
    }
}

#[derive(Debug)]
pub enum RenameError {
    Exists,
    CrossDevice,
    Io(std::io::Error),
}

impl std::fmt::Display for RenameError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RenameError::Exists => f.write_str("destination already exists"),
            RenameError::CrossDevice => f.write_str("source and destination are on different filesystems"),
            RenameError::Io(e) => e.fmt(f),
        }
    }
}

impl std::error::Error for RenameError {}

/// How a file got to its destination, and its full hash if that was computed
/// on the way (copies hash what they copy).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Transferred {
    pub method: Method,
    pub hash: Option<String>,
}

/// Move or copy `src` to `dst` (which must not exist). A copy is verified
/// against the source as it was read; `expected_hash`, if known, is the full
/// hash `src` should have, and a source that doesn't match it is not copied.
pub fn transfer(
    db: &Db,
    run_id: &str,
    src: &Path,
    dst: &Path,
    mode: Mode,
    expected_hash: Option<&str>,
) -> Result<Transferred> {
    if let Some(parent) = dst.parent() {
        fs::create_dir_all(parent).with_context(|| format!("can't create directory {}", parent.display()))?;
    }
    if mode == Mode::Move {
        match rename_noreplace(src, dst) {
            Ok(()) => {
                fsync_parent(dst)?;
                fsync_parent(src)?;
                return Ok(Transferred { method: Method::Rename, hash: None });
            }
            Err(RenameError::CrossDevice) => {} // fall through to a verified copy
            Err(RenameError::Exists) => bail!("{} already exists", dst.display()),
            Err(RenameError::Io(e)) => {
                return Err(e).with_context(|| format!("can't move {} to {}", src.display(), dst.display()))
            }
        }
    }

    let tmp = tmp_path_for(dst);
    let op = if mode == Mode::Move { "move" } else { "copy" };
    let jid = db.journal_begin(run_id, op, src, dst, &tmp, expected_hash.unwrap_or(""))?;
    let placed = (|| -> Result<String> {
        let hash = copy_verified(src, &tmp, expected_hash)?;
        match rename_noreplace(&tmp, dst) {
            Ok(()) => {}
            Err(RenameError::Exists) => bail!("{} already exists", dst.display()),
            Err(e) => return Err(e).context("can't put the verified copy in place"),
        }
        fsync_parent(dst)?;
        Ok(hash)
    })();
    let hash = match placed {
        Ok(h) => h,
        Err(e) => {
            let _ = fs::remove_file(&tmp);
            db.journal_set_state(jid, "rolled-back")?;
            return Err(e).with_context(|| format!("can't copy {} to {}", src.display(), dst.display()));
        }
    };
    db.journal_set_state(jid, "placed")?;

    if mode == Mode::Move {
        fs::remove_file(src)
            .with_context(|| format!("copied to {} but can't remove the original {}", dst.display(), src.display()))?;
        fsync_parent(src)?;
    }
    db.journal_set_state(jid, "done")?;
    Ok(Transferred { method: Method::Copy, hash: Some(hash) })
}

/// Copy `src` to `tmp`, hashing the source as it's read, then read the copy
/// back from disk and check it hashes the same. Returns the hash.
fn copy_verified(src: &Path, tmp: &Path, expected_hash: Option<&str>) -> Result<String> {
    use std::io::{Read, Write};

    use crate::ui::{self, Lane};
    if tmp.symlink_metadata().is_ok() {
        fs::remove_file(tmp)?; // stale leftover of an earlier crash
    }
    let mut from = File::open(src)?;
    let size = from.metadata()?.len();
    let mtime = from.metadata()?.modified()?;
    let mut to = fs::OpenOptions::new().write(true).create_new(true).open(tmp)?;
    let mut h = xxhash_rust::xxh3::Xxh3::new();
    let mut buf = vec![0u8; 1 << 20];
    let mut done = 0u64;
    loop {
        let n = from.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
        to.write_all(&buf[..n])?;
        done += n as u64;
        ui::lane_note(Lane::File, format!("copying {} ({}%)", ui::size(size), done * 100 / size.max(1)));
    }
    let source_hash = format!("{:032x}", h.digest128());
    if expected_hash.is_some_and(|e| e != source_hash) {
        bail!("the original changed since it was scanned; not copying it");
    }
    ui::lane_note(Lane::File, "flushing the copy to disk");
    to.set_modified(mtime)?;
    to.sync_all()?;
    drop(to);
    let got = crate::hash::hash_file_with(tmp, false, &|done| {
        ui::lane_note(Lane::File, format!("verifying the copy ({}%)", done * 100 / size.max(1)));
    })?;
    ui::lane_note(Lane::File, "");
    if got != source_hash {
        bail!("the copy does not match the original (the disk may be failing)");
    }
    Ok(source_hash)
}

/// `dst`, or `name [2].ext`, `name [3].ext`, ... whichever doesn't exist yet.
pub fn unique_dest(dst: &Path) -> PathBuf {
    if dst.symlink_metadata().is_err() {
        return dst.to_path_buf();
    }
    let stem = dst.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let ext = dst.extension().map(|e| format!(".{}", e.to_string_lossy())).unwrap_or_default();
    (2..)
        .map(|n| dst.with_file_name(format!("{stem} [{n}]{ext}")))
        .find(|p| p.symlink_metadata().is_err())
        .expect("unbounded range")
}

/// Is `candidate` a `name [N].ext` variant of `base` (see [`unique_dest`])?
pub fn is_numbered_variant(candidate: &Path, base: &Path) -> bool {
    if candidate.parent() != base.parent() || candidate.extension() != base.extension() {
        return false;
    }
    let (Some(c), Some(b)) = (
        candidate.file_stem().and_then(|s| s.to_str()),
        base.file_stem().and_then(|s| s.to_str()),
    ) else {
        return false;
    };
    c.strip_prefix(b)
        .and_then(|r| r.strip_prefix(" ["))
        .and_then(|r| r.strip_suffix(']'))
        .is_some_and(|n| n.parse::<u32>().is_ok_and(|n| n >= 2))
}

/// Remove `dir` and its parents while they are empty, stopping at `stop_at`
/// (which itself is never removed).
pub fn prune_empty_dirs(dir: &Path, stop_at: &Path) {
    let mut dir = dir.to_path_buf();
    while dir != stop_at && dir.starts_with(stop_at) {
        match fs::remove_dir(&dir) {
            Ok(()) => info!("removed empty directory {}", crate::ui::show_outside(&dir)),
            Err(_) => break, // not empty (or gone): stop
        }
        if !dir.pop() {
            break;
        }
    }
}

/// Finish or roll back operations interrupted by a crash.
pub fn recover(db: &Db) -> Result<usize> {
    let pending = db.journal_pending()?;
    for j in &pending {
        warn!(id = j.id, op = %j.op, state = %j.state, src = %j.src.display(), dst = %j.dst.display(),
            "found an unfinished file operation from an earlier run, resolving it");
        let outcome = match j.state.as_str() {
            "started" => {
                // The destination was never created; the source is untouched.
                if j.tmp.symlink_metadata().is_ok() {
                    fs::remove_file(&j.tmp)?;
                }
                "rolled back: removed the partial copy, the original is untouched".to_string()
            }
            "placed" if j.op == "move" && j.src.exists() => {
                let src_hash = hash_file(&j.src, false)?;
                // The journal has the expected hash if it was known up front;
                // otherwise the two files just have to be identical.
                let src_same = j.hash.is_empty() || src_hash == j.hash;
                let dst_ok = j.dst.exists() && hash_file(&j.dst, false)? == src_hash;
                if dst_ok && src_same {
                    fs::remove_file(&j.src)?;
                    "completed: the copy was verified, removed the original".to_string()
                } else {
                    "left both files in place: they no longer match, please check them".to_string()
                }
            }
            _ => "nothing left to do".to_string(),
        };
        warn!(id = j.id, outcome = %outcome, "resolved unfinished file operation");
        crate::ui::line(format!("recovered interrupted {} of {}: {outcome}", j.op, j.src.display()));
        db.journal_set_state(j.id, "recovered")?;
    }
    Ok(pending.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() -> (tempfile::TempDir, Db) {
        let d = tempfile::tempdir().unwrap();
        let db = Db::open(&d.path().join("db.sqlite")).unwrap();
        (d, db)
    }

    #[test]
    fn move_never_overwrites() {
        let (d, db) = setup();
        let (a, b) = (d.path().join("a.mp3"), d.path().join("b.mp3"));
        fs::write(&a, b"aaa").unwrap();
        fs::write(&b, b"bbb").unwrap();
        let h = hash_file(&a, false).unwrap();
        assert!(transfer(&db, "t", &a, &b, Mode::Move, Some(&h)).is_err());
        assert_eq!(fs::read(&a).unwrap(), b"aaa");
        assert_eq!(fs::read(&b).unwrap(), b"bbb");
    }

    #[test]
    fn move_and_copy() {
        let (d, db) = setup();
        let a = d.path().join("a.mp3");
        fs::write(&a, b"aaa").unwrap();
        let h = hash_file(&a, false).unwrap();
        let c = d.path().join("x/y/c.mp3");
        let t = transfer(&db, "t", &a, &c, Mode::Copy, None).unwrap();
        assert_eq!(t, Transferred { method: Method::Copy, hash: Some(h.clone()) });
        assert!(a.exists());
        assert_eq!(fs::read(&c).unwrap(), b"aaa");
        let c2 = d.path().join("x/y/c2.mp3");
        assert_eq!(transfer(&db, "t", &a, &c2, Mode::Copy, Some(&h)).unwrap().method, Method::Copy);
        let m = d.path().join("z/m.mp3");
        assert_eq!(transfer(&db, "t", &a, &m, Mode::Move, None).unwrap().method, Method::Rename);
        assert!(!a.exists());
        assert_eq!(fs::read(&m).unwrap(), b"aaa");
        assert!(db.journal_pending().unwrap().is_empty());
    }

    #[test]
    fn copy_with_wrong_hash_is_discarded() {
        let (d, db) = setup();
        let a = d.path().join("a.mp3");
        fs::write(&a, b"aaa").unwrap();
        let c = d.path().join("c.mp3");
        assert!(transfer(&db, "t", &a, &c, Mode::Copy, Some("0000")).is_err());
        assert!(!c.exists());
        assert!(!tmp_path_for(&c).exists());
        assert!(a.exists());
    }

    #[test]
    fn unique_dest_numbers() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("t.flac");
        assert_eq!(unique_dest(&p), p);
        fs::write(&p, b"").unwrap();
        let p2 = unique_dest(&p);
        assert_eq!(p2, d.path().join("t [2].flac"));
        assert!(is_numbered_variant(&p2, &p));
        assert!(!is_numbered_variant(&p, &p));
        assert!(!is_numbered_variant(&d.path().join("t [x].flac"), &p));
        assert!(!is_numbered_variant(&d.path().join("t [2].mp3"), &p));
    }

    #[test]
    fn recover_rolls_back_started_copy() {
        let (d, db) = setup();
        let (src, dst) = (d.path().join("s.mp3"), d.path().join("d.mp3"));
        fs::write(&src, b"sss").unwrap();
        let tmp = tmp_path_for(&dst);
        fs::write(&tmp, b"ss").unwrap(); // partial copy
        db.journal_begin("t", "move", &src, &dst, &tmp, &hash_file(&src, false).unwrap()).unwrap();
        assert_eq!(recover(&db).unwrap(), 1);
        assert!(!tmp.exists());
        assert!(src.exists());
        assert!(!dst.exists());
        assert!(db.journal_pending().unwrap().is_empty());
    }

    #[test]
    fn recover_completes_placed_move() {
        let (d, db) = setup();
        let (src, dst) = (d.path().join("s.mp3"), d.path().join("d.mp3"));
        fs::write(&src, b"sss").unwrap();
        fs::write(&dst, b"sss").unwrap();
        let h = hash_file(&src, false).unwrap();
        let id = db.journal_begin("t", "move", &src, &dst, &tmp_path_for(&dst), &h).unwrap();
        db.journal_set_state(id, "placed").unwrap();
        recover(&db).unwrap();
        assert!(!src.exists());
        assert_eq!(fs::read(&dst).unwrap(), b"sss");
    }

    #[test]
    fn recover_keeps_mismatched_files() {
        let (d, db) = setup();
        let (src, dst) = (d.path().join("s.mp3"), d.path().join("d.mp3"));
        fs::write(&src, b"sss").unwrap();
        fs::write(&dst, b"xxx").unwrap();
        let id = db
            .journal_begin("t", "move", &src, &dst, &tmp_path_for(&dst), &hash_file(&src, false).unwrap())
            .unwrap();
        db.journal_set_state(id, "placed").unwrap();
        recover(&db).unwrap();
        assert!(src.exists());
        assert!(dst.exists());
    }

    #[test]
    fn recover_without_a_known_hash_compares_the_files() {
        let (d, db) = setup();
        for (same, name) in [(true, "a"), (false, "b")] {
            let (src, dst) = (d.path().join(format!("{name}.src")), d.path().join(format!("{name}.dst")));
            fs::write(&src, b"sss").unwrap();
            fs::write(&dst, if same { b"sss" } else { b"xxx" }).unwrap();
            let id = db.journal_begin("t", "move", &src, &dst, &tmp_path_for(&dst), "").unwrap();
            db.journal_set_state(id, "placed").unwrap();
            recover(&db).unwrap();
            assert_eq!(src.exists(), !same, "{name}");
            assert!(dst.exists());
        }
    }
}
