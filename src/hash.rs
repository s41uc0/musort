use std::fs::File;
use std::io::Read;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{bail, Context, Result};
use xxhash_rust::xxh3::Xxh3;

/// Content hash (xxh3-128, hex). Used to detect changes and exact duplicates
/// within the collection, not as a defense against tampering, so a fast
/// non-cryptographic hash is the right tool.
///
/// When `cancellable`, gives up as soon as Ctrl+C is pressed (hashing is
/// read-only, so that's always safe).
pub fn hash_file(path: &Path, cancellable: bool) -> Result<String> {
    hash_file_with(path, cancellable, &|_| {})
}

/// Bytes hashed by this process so far (for throughput in the status bar).
pub static HASHED_BYTES: AtomicU64 = AtomicU64::new(0);

/// [`hash_file`], calling `progress` with the number of bytes read so far.
pub fn hash_file_with(path: &Path, cancellable: bool, progress: &dyn Fn(u64)) -> Result<String> {
    let mut f = File::open(path).with_context(|| format!("can't open {}", path.display()))?;
    let mut h = Xxh3::new();
    let mut buf = vec![0u8; 1 << 20];
    let mut done = 0u64;
    loop {
        if cancellable && crate::guard::stop_requested() {
            bail!(Cancelled);
        }
        let n = f.read(&mut buf).with_context(|| format!("can't read {}", path.display()))?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
        done += n as u64;
        HASHED_BYTES.fetch_add(n as u64, Ordering::Relaxed);
        progress(done);
    }
    Ok(format!("{:032x}", h.digest128()))
}

/// How much of each end of a file the quick fingerprint reads.
pub const QUICK_BLOCK: u64 = 1 << 20;

/// A file's quick fingerprint, and its full hash when the fingerprint
/// happened to read the whole file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Quick {
    pub quick: String,
    pub full: Option<String>,
}

/// Quick fingerprint: xxh3-128 over the size, the first [`QUICK_BLOCK`] and
/// the last [`QUICK_BLOCK`] bytes. Tags and audio frames sit at those ends,
/// so any real change to a file changes it, at a fraction of the cost of
/// reading everything. Equal fingerprints are only a *candidate* for
/// identical content: confirm with [`hash_file`] before acting on it.
/// Files up to two blocks are read whole, so their full hash comes free.
pub fn quick_hash(path: &Path, cancellable: bool) -> Result<Quick> {
    use std::io::{Seek, SeekFrom};
    let mut f = File::open(path).with_context(|| format!("can't open {}", path.display()))?;
    let size = f.metadata()?.len();
    let mut q = Xxh3::new();
    q.update(&size.to_le_bytes());
    let read = |f: &mut File, len: u64, q: &mut Xxh3, full: Option<&mut Xxh3>| -> Result<()> {
        if cancellable && crate::guard::stop_requested() {
            bail!(Cancelled);
        }
        let mut buf = vec![0u8; len as usize];
        f.read_exact(&mut buf).with_context(|| format!("can't read {}", path.display()))?;
        q.update(&buf);
        if let Some(full) = full {
            full.update(&buf);
        }
        HASHED_BYTES.fetch_add(len, Ordering::Relaxed);
        Ok(())
    };
    if size <= 2 * QUICK_BLOCK {
        let mut full = Xxh3::new();
        read(&mut f, size, &mut q, Some(&mut full))?;
        return Ok(Quick { quick: format!("{:032x}", q.digest128()), full: Some(format!("{:032x}", full.digest128())) });
    }
    read(&mut f, QUICK_BLOCK, &mut q, None)?;
    f.seek(SeekFrom::Start(size - QUICK_BLOCK))?;
    read(&mut f, QUICK_BLOCK, &mut q, None)?;
    Ok(Quick { quick: format!("{:032x}", q.digest128()), full: None })
}

#[derive(Debug)]
pub struct Cancelled;

impl std::fmt::Display for Cancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("cancelled")
    }
}

impl std::error::Error for Cancelled {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_content_same_hash() {
        let d = tempfile::tempdir().unwrap();
        let (a, b, c) = (d.path().join("a"), d.path().join("b"), d.path().join("c"));
        std::fs::write(&a, b"hello").unwrap();
        std::fs::write(&b, b"hello").unwrap();
        std::fs::write(&c, b"hellp").unwrap();
        let ha = hash_file(&a, false).unwrap();
        assert_eq!(ha.len(), 32);
        assert_eq!(ha, hash_file(&b, false).unwrap());
        assert_ne!(ha, hash_file(&c, false).unwrap());
    }

    #[test]
    fn quick_hash_small_and_large() {
        let d = tempfile::tempdir().unwrap();
        // Small: read whole, full hash included and equal to hash_file.
        let s = d.path().join("s");
        std::fs::write(&s, b"hello").unwrap();
        let q = quick_hash(&s, false).unwrap();
        assert_eq!(q.full, Some(hash_file(&s, false).unwrap()));

        // Large: only the ends count. A change in the middle is invisible to
        // it (that's what the full hash on a collision is for); a change at
        // either end, or in size, is not.
        let big = |name: &str, mid: u8, end: u8, extra: usize| {
            let mut v = vec![7u8; 3 * QUICK_BLOCK as usize + extra];
            let n = v.len();
            v[n / 2] = mid;
            v[n - 1] = end;
            let p = d.path().join(name);
            std::fs::write(&p, v).unwrap();
            quick_hash(&p, false).unwrap()
        };
        let a = big("a", 1, 1, 0);
        assert_eq!(a.full, None);
        assert_eq!(a.quick, big("b", 2, 1, 0).quick);
        assert_ne!(a.quick, big("c", 1, 2, 0).quick);
        assert_ne!(a.quick, big("e", 1, 1, 1).quick);
    }
}
