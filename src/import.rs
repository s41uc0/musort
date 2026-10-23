use std::collections::HashMap;
use std::path::Path;

use anyhow::{bail, Context, Result};

use crate::cli::RunArgs;
use crate::collection::{Collection, MARKER};
use crate::config::Config;
use crate::db::{Db, WorkRun};
use crate::fsops::Mode;
use crate::history::History;
use crate::pipeline::{Origin, Pipeline, Stats};
use crate::scan::{self, scan, ScanOptions, Scanned};
use crate::ui;

#[allow(clippy::too_many_arguments)]
pub fn import(
    coll: &Collection,
    cfg: &Config,
    db: &Db,
    run_id: &str,
    hist: &History,
    src: &Path,
    mode: Mode,
    args: &RunArgs,
) -> Result<Stats> {
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
    ui::busy("loading", "reading what's known about the collection from the database (to spot duplicates)");
    let known: HashMap<_, _> = db.all_tracks()?.into_iter().map(|(rel, row)| (coll.root.join(rel), row)).collect();
    let mut p = Pipeline::new(coll, cfg, db, run_id, hist, args, Origin::Import { src_root: src.clone(), mode }, &known)?;
    drop(known);
    ui::finish();
    let shown = ui::show_path(&coll.root, &src);
    let no_cache = HashMap::new();

    // `--quick`: pick up an interrupted import of the same folder (same mode)
    // without scanning it again.
    let interrupted = match db.worklist_load()? {
        Some((w, items))
            if args.quick
                && w.command == "import"
                && w.source.as_deref() == Some(src.as_path())
                && w.copy == (mode == Mode::Copy)
                && items.iter().any(|i| !i.done) =>
        {
            Some((w, items))
        }
        _ => None,
    };
    let (files, errors) = match interrupted {
        Some((w, items)) => {
            let left = items.iter().filter(|i| !i.done).count();
            ui::say_plain(format!(
                "quick: picking up the import of {} where it stopped: {left} of {} file(s) left",
                w.run_id,
                items.len()
            ));
            let r = scan::resume(items, &src, &no_cache, None);
            if r.cancelled {
                return Ok(Stats { interrupted: true, ..Default::default() });
            }
            if !r.gone.is_empty() {
                ui::say_plain(format!("{} file(s) are gone from {shown} since; skipped", r.gone.len()));
            }
            p.album_context(&r.all);
            (r.todo, r.errors)
        }
        None => {
            if args.quick {
                ui::say_plain(format!("quick: no interrupted import of {shown} to pick up, so scanning it"));
            }
            if !args.dry_run {
                db.worklist_clear()?;
            }
            // Import sources aren't in the collection, so nothing is stored.
            let scanned = scan(&src, &ScanOptions { skip: &[], cache: &no_cache, clean_leftovers: false, store: None });
            if scanned.cancelled {
                return Ok(Stats { interrupted: true, ..Default::default() });
            }
            if scanned.files.is_empty() && scanned.errors.is_empty() {
                ui::say_plain(format!("no audio files found in {shown}"));
                return Ok(Stats::default());
            }
            let mut found = format!("found {} audio file(s) in {shown}", scanned.files.len());
            if !scanned.non_audio.is_empty() {
                found.push_str(&format!("; {} other file(s) (covers, playlists, ...) stay there", scanned.non_audio.len()));
            }
            ui::say_plain(found);
            if !args.dry_run {
                let work = WorkRun {
                    run_id: run_id.to_string(),
                    command: "import".into(),
                    source: Some(src.clone()),
                    copy: mode == Mode::Copy,
                };
                ui::busy("saving", format!("remembering the list of {} file(s), so -q can pick up from here", scanned.files.len()));
                db.worklist_save(&work, &scanned.files.iter().map(Scanned::work_item).collect::<Vec<_>>())?;
                ui::finish();
            }
            (scanned.files, scanned.errors)
        }
    };

    p.stats.errors += errors.len();
    p.run(files)?;
    if !args.dry_run && !p.stats.interrupted {
        db.worklist_clear()?;
    }
    Ok(p.stats)
}
