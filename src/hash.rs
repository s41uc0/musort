use std::fs::File;
use std::io::Read;
use std::path::Path;

use anyhow::{bail, Context, Result};
use xxhash_rust::xxh3::Xxh3;

/// Content hash (xxh3-128, hex). Used to detect changes and exact duplicates
/// within the collection, not as a defense against tampering, so a fast
/// non-cryptographic hash is the right tool.
///
/// When `cancellable`, gives up as soon as Ctrl+C is pressed (hashing is
/// read-only, so that's always safe).
pub fn hash_file(path: &Path, cancellable: bool) -> Result<String> {
    let mut f = File::open(path).with_context(|| format!("can't open {}", path.display()))?;
    let mut h = Xxh3::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        if cancellable && crate::guard::stop_requested() {
            bail!(Cancelled);
        }
        let n = f.read(&mut buf).with_context(|| format!("can't read {}", path.display()))?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(format!("{:032x}", h.digest128()))
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
}
