use std::collections::HashMap;
use std::path::Path;

use anyhow::{bail, Context, Result};

use crate::cli::RunArgs;
use crate::collection::{Collection, MARKER};
use crate::config::Config;
use crate::db::Db;
use crate::fsops::Mode;
use crate::pipeline::{Origin, Pipeline, Stats};
use crate::scan::{scan, ScanOptions};
use crate::ui;

pub fn import(coll: &Collection, cfg: &Config, db: &Db, run_id: &str, src: &Path, mode: Mode, args: &RunArgs) -> Result<Stats> {
    let src = std::fs::canonicalize(src).with_context(|| format!("{} does not exist", src.display()))?;
    if src.starts_with(&coll.root) {
        bail!(
            "{} is inside the collection; use `musort organize` to sort files that are already in it",
            src.display()
        );
    }
    if coll.root.starts_with(&src) {
        bail!("{} contains the collection itself; pick the folder with the new music", src.display());
    }
    if src.join(MARKER).is_dir() {
        bail!("{} is another musort collection; importing whole collections is not supported", src.display());
    }

    // Set up first, so configuration problems surface before a long scan.
    let mut p = Pipeline::new(coll, cfg, db, run_id, args, Origin::Import { src_root: src.clone(), mode })?;
    let no_cache = HashMap::new();
    let scanned = scan(&src, &ScanOptions { skip: &[], cache: &no_cache, clean_leftovers: false });
    if scanned.cancelled {
        return Ok(Stats { interrupted: true, ..Default::default() });
    }
    if scanned.files.is_empty() && scanned.errors.is_empty() {
        ui::say(format!("no audio files found in {}", src.display()));
        return Ok(Stats::default());
    }
    ui::say(format!("found {} audio file(s) in {}", scanned.files.len(), src.display()));

    p.stats.errors += scanned.errors.len();
    p.run(scanned.files)?;
    for l in p.summary_lines() {
        ui::say(&l);
    }
    if !scanned.non_audio.is_empty() {
        let l = format!(
            "{} non-audio file(s) (covers, playlists, ...) were left in {}",
            scanned.non_audio.len(),
            src.display()
        );
        ui::say(&l);
    }
    Ok(p.stats)
}
