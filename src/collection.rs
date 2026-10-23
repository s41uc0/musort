use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

pub const MARKER: &str = ".musort";

/// A musort collection: a directory with a `.musort/` directory in it.
#[derive(Debug, Clone)]
pub struct Collection {
    /// Canonical, absolute collection root.
    pub root: PathBuf,
}

impl Collection {
    pub fn dir(&self) -> PathBuf {
        self.root.join(MARKER)
    }
    pub fn db_path(&self) -> PathBuf {
        self.dir().join("database.db")
    }
    pub fn config_path(&self) -> PathBuf {
        self.dir().join("config.toml")
    }
    /// One readable `.log` and one `.jsonl` history per run.
    pub fn logs_dir(&self) -> PathBuf {
        self.dir().join("logs")
    }

    /// Find the collection for `start` (or the current directory): the nearest
    /// ancestor that contains `.musort/`.
    pub fn find(start: Option<&Path>) -> Result<Self> {
        let start = match start {
            Some(p) => p.to_path_buf(),
            None => std::env::current_dir().context("can't determine the current directory")?,
        };
        let canon = fs::canonicalize(&start)
            .with_context(|| format!("{} does not exist", start.display()))?;
        match find_marker(&canon) {
            Some(root) => Ok(Collection { root }),
            None => bail!(
                "{} is not inside a musort collection (no {MARKER}/ directory found here or above).\n\
                 Run `musort init` in the directory that should hold your music first.",
                canon.display()
            ),
        }
    }

    /// Create a new collection at `path`.
    pub fn init(path: &Path) -> Result<Self> {
        fs::create_dir_all(path).with_context(|| format!("can't create {}", path.display()))?;
        let root = fs::canonicalize(path)?;
        if root.join(MARKER).exists() {
            bail!("{} is already a musort collection", root.display());
        }
        if let Some(parent) = root.parent().and_then(find_marker) {
            bail!(
                "{} is inside the collection at {}; nested collections are not allowed",
                root.display(),
                parent.display()
            );
        }
        fs::create_dir(root.join(MARKER))?;
        let c = Collection { root };
        fs::write(c.config_path(), crate::config::DEFAULT_CONFIG)?;
        Ok(c)
    }

    /// Take the collection's exclusive lock, so two musort processes never
    /// work on the same collection at once. Held until the returned file drops.
    pub fn lock(&self) -> Result<File> {
        use rustix::fs::{flock, FlockOperation};
        let f = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(self.dir().join("lock"))?;
        if flock(&f, FlockOperation::NonBlockingLockExclusive).is_err() {
            bail!(
                "another musort process is already working on {}; wait for it to finish",
                self.root.display()
            );
        }
        Ok(f)
    }

    /// Path relative to the collection root, if `abs` is inside it.
    pub fn rel<'a>(&self, abs: &'a Path) -> Option<&'a Path> {
        abs.strip_prefix(&self.root).ok()
    }
}

fn find_marker(from: &Path) -> Option<PathBuf> {
    from.ancestors()
        .find(|dir| dir.join(MARKER).is_dir())
        .map(Path::to_path_buf)
}
