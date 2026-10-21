use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use anyhow::Result;
use tracing::info;

use crate::cli::RunArgs;
use crate::collection::Collection;
use crate::config::Config;
use crate::db::Db;
use crate::pipeline::{Origin, Pipeline, Stats};
use crate::scan::{scan, ScanOptions};
use crate::ui;

pub fn organize(coll: &Collection, cfg: &Config, db: &Db, run_id: &str, args: &RunArgs) -> Result<Stats> {
    let rows = db.all_tracks()?;
    let cache: HashMap<PathBuf, _> = rows.into_iter().map(|(rel, row)| (coll.root.join(rel), row)).collect();
    // Set up first, so configuration problems surface before a long scan.
    let mut p = Pipeline::new(coll, cfg, db, run_id, args, Origin::Collection)?;
    let skip = vec![coll.dir(), coll.root.join(&cfg.layout.duplicates_dir)];

    let scanned = scan(
        &coll.root,
        &ScanOptions { skip: &skip, cache: &cache, clean_leftovers: !args.dry_run },
    );
    if scanned.cancelled {
        return Ok(Stats { interrupted: true, ..Default::default() });
    }
    let cached = scanned.files.iter().filter(|f| f.from_cache).count();
    ui::say(format!(
        "found {} audio file(s) ({} unchanged since the last run)",
        scanned.files.len(),
        cached
    ));
    if !scanned.non_audio.is_empty() {
        info!("{} non-audio file(s) in the collection were left alone", scanned.non_audio.len());
    }

    // The database is only a cache: forget rows whose file is gone.
    let on_disk: HashSet<&PathBuf> = scanned.files.iter().map(|f| &f.abs).collect();
    let unreadable: HashSet<&PathBuf> = scanned.errors.iter().map(|(p, _)| p).collect();
    let by_hash: HashMap<&str, &PathBuf> = scanned.files.iter().map(|f| (f.hash.as_str(), &f.abs)).collect();
    for (abs, row) in &cache {
        if on_disk.contains(abs) || unreadable.contains(abs) || skip.iter().any(|s| abs.starts_with(s)) {
            continue;
        }
        match by_hash.get(row.hash.as_str()) {
            Some(now) => info!(was = %abs.display(), now = %now.display(), "file was moved outside musort"),
            None => info!(path = %abs.display(), track = %row.info.meta.describe(), "file is gone from disk, forgetting it"),
        }
        if !args.dry_run {
            db.delete_track(&row.path)?;
        }
    }

    p.stats.errors += scanned.errors.len();
    p.run(scanned.files)?;
    for l in p.summary_lines() {
        ui::say(&l);
    }
    Ok(p.stats)
}
