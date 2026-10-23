use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use anyhow::Result;
use tracing::info;

use crate::cli::RunArgs;
use crate::collection::Collection;
use crate::config::Config;
use crate::db::{Db, TrackRow, WorkItem, WorkRun};
use crate::history::History;
use crate::pipeline::{Origin, Pipeline, Stats};
use crate::scan::{self, scan, ScanOptions, Scanned, Store};
use crate::ui;

pub fn organize(coll: &Collection, cfg: &Config, db: &Db, run_id: &str, hist: &History, args: &RunArgs) -> Result<Stats> {
    ui::busy("loading", "reading what's known about the collection from the database");
    let rows = db.all_tracks()?;
    info!("{} file(s) known from the database", rows.len());
    let cache: HashMap<PathBuf, _> = rows.into_iter().map(|(rel, row)| (coll.root.join(rel), row)).collect();
    // Set up first, so configuration problems surface before a long scan.
    let mut p = Pipeline::new(coll, cfg, db, run_id, hist, args, Origin::Collection, &cache)?;
    ui::finish();

    let work = WorkRun { run_id: run_id.to_string(), command: "organize".into(), source: None, copy: false };
    // What scanning learns goes straight to the database (not in a dry run).
    let store = (!args.dry_run).then(|| Store { queue: db.queue(), root: coll.root.clone() });
    let found = if args.quick {
        quick(coll, db, args, &cache, &work, store.as_ref(), &mut p)?
    } else {
        full_scan(coll, cfg, db, args, &cache, store)?
    };
    let Some((files, errors)) = found else {
        return Ok(Stats { interrupted: true, ..Default::default() });
    };
    if !args.quick && !args.dry_run {
        ui::busy("saving", format!("remembering the list of {} file(s), so -q can pick up from here", files.len()));
        db.worklist_save(&work, &files.iter().map(Scanned::work_item).collect::<Vec<_>>())?;
        ui::finish();
    }

    p.stats.errors += errors.len();
    p.run(files)?;
    if !args.dry_run && !p.stats.interrupted {
        db.worklist_clear()?;
    }
    Ok(p.stats)
}

type Found = (Vec<Scanned>, Vec<(PathBuf, String)>);

/// Walk the whole collection. None if interrupted.
fn full_scan(
    coll: &Collection,
    cfg: &Config,
    db: &Db,
    args: &RunArgs,
    cache: &HashMap<PathBuf, TrackRow>,
    store: Option<Store>,
) -> Result<Option<Found>> {
    if !args.dry_run {
        db.worklist_clear()?;
    }
    let skip = vec![coll.dir(), coll.root.join(&cfg.layout.duplicates_dir)];
    let scanned = scan(&coll.root, &ScanOptions { skip: &skip, cache, clean_leftovers: !args.dry_run, store });
    if scanned.cancelled {
        return Ok(None);
    }
    let cached = scanned.files.iter().filter(|f| f.from_cache).count();
    let mut found = format!("found {} audio file(s), {cached} unchanged since the last run", scanned.files.len());
    if !scanned.non_audio.is_empty() {
        found.push_str(&format!("; {} other file(s) left alone", scanned.non_audio.len()));
    }
    ui::say_plain(found);

    // The database is only a cache: forget rows whose file is gone.
    ui::busy("checking", "forgetting database entries of files that are gone");
    let on_disk: HashSet<&PathBuf> = scanned.files.iter().map(|f| &f.abs).collect();
    let unreadable: HashSet<&PathBuf> = scanned.errors.iter().map(|(p, _)| p).collect();
    let by_quick: HashMap<&str, &PathBuf> = scanned.files.iter().map(|f| (f.quick.as_str(), &f.abs)).collect();
    let mut forget = vec![];
    for (abs, row) in cache {
        if on_disk.contains(abs) || unreadable.contains(abs) || skip.iter().any(|s| abs.starts_with(s)) {
            continue;
        }
        match row.quick.as_deref().and_then(|q| by_quick.get(q)) {
            Some(now) => info!(was = %abs.display(), now = %now.display(), "file was moved outside musort"),
            None => info!(path = %abs.display(), track = %row.info.meta.describe(), "file is gone from disk, forgetting it"),
        }
        forget.push(row.path.as_path());
    }
    if !forget.is_empty() {
        ui::say_plain(format!("{} file(s) the database knew about are gone (or were moved outside musort); forgotten", forget.len()));
        if !args.dry_run {
            for f in forget {
                db.delete_track(f);
            }
        }
    }
    ui::finish();
    Ok(Some((scanned.files, scanned.errors)))
}

/// `--quick`: no walk. Continue an interrupted organize, or else re-check the
/// files the database knows about.
fn quick(
    coll: &Collection,
    db: &Db,
    args: &RunArgs,
    cache: &HashMap<PathBuf, TrackRow>,
    work: &WorkRun,
    store: Option<&Store>,
    p: &mut Pipeline<'_>,
) -> Result<Option<Found>> {
    let (items, continuing) = match db.worklist_load()? {
        Some((w, items)) if w.command == "organize" && items.iter().any(|i| !i.done) => {
            let left = items.iter().filter(|i| !i.done).count();
            ui::say_plain(format!(
                "quick: picking up the organize of {} where it stopped: {left} of {} file(s) left",
                w.run_id,
                items.len()
            ));
            (items, true)
        }
        _ => {
            ui::say_plain(format!(
                "quick: checking the {} file(s) musort knows about, without scanning (files added since aren't noticed)",
                cache.len()
            ));
            let items = cache
                .iter()
                .map(|(abs, r)| WorkItem {
                    path: abs.clone(),
                    size: r.size,
                    mtime_ns: r.mtime_ns,
                    quick: r.quick.clone(),
                    hash: r.hash.clone(),
                    info: r.info.clone(),
                    done: false,
                })
                .collect();
            (items, false)
        }
    };
    let r = scan::resume(items, &coll.root, cache, store);
    if r.cancelled {
        return Ok(None);
    }
    if !r.gone.is_empty() {
        ui::say_plain(format!("{} file(s) are gone since; skipped and forgotten", r.gone.len()));
        if !args.dry_run {
            for rel in r.gone.iter().filter_map(|g| coll.rel(g)) {
                db.delete_track(rel);
            }
        }
    }
    if !continuing && !args.dry_run {
        ui::busy("saving", format!("remembering the list of {} file(s), so -q can pick up from here", r.todo.len()));
        db.worklist_save(work, &r.todo.iter().map(Scanned::work_item).collect::<Vec<_>>())?;
        ui::finish();
    }
    p.album_context(&r.all);
    Ok(Some((r.todo, r.errors)))
}
