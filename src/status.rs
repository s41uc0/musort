use std::collections::{BTreeMap, HashMap};

use anyhow::Result;

use crate::collection::Collection;
use crate::db::{Db, Status};

pub fn status(coll: &Collection, db: &Db) -> Result<()> {
    let rows = db.all_tracks()?;
    println!("collection: {}", coll.root.display());

    let mut by: BTreeMap<String, usize> = BTreeMap::new();
    let mut unidentified = vec![];
    let mut new = 0;
    let mut recordings: HashMap<&str, Vec<&std::path::Path>> = HashMap::new();
    for row in rows.values() {
        match row.status {
            Status::Organized => {
                *by.entry(row.identified_by.clone().unwrap_or_else(|| "?".into())).or_default() += 1;
                if let Some(id) = row.info.meta.mb_recording_id.as_deref() {
                    recordings.entry(id).or_default().push(&row.path);
                }
            }
            Status::Unidentified => unidentified.push(&row.path),
            Status::New => new += 1,
        }
    }
    let organized: usize = by.values().sum();
    let detail: Vec<String> = by.iter().map(|(k, v)| format!("{v} via {k}")).collect();
    println!(
        "tracks: {organized} organized ({}), {} unidentified",
        if detail.is_empty() { "none".into() } else { detail.join(", ") },
        unidentified.len()
    );
    if new > 0 {
        println!("{new} file(s) found by a scan but not sorted yet (run `musort organize`, or -q to pick up where it stopped)");
    }

    if !unidentified.is_empty() {
        unidentified.sort();
        println!("\nunidentified (fix their tags, or enable AcoustID, then run `musort organize`):");
        for p in unidentified.iter().take(25) {
            println!("  {}", p.display());
        }
        if unidentified.len() > 25 {
            println!("  ... and {} more", unidentified.len() - 25);
        }
    }

    let mut dups: Vec<Vec<&std::path::Path>> = recordings.into_values().filter(|v| v.len() > 1).collect();
    if !dups.is_empty() {
        dups.sort();
        println!("\npossible duplicates (same recording, different files):");
        for group in dups.iter().take(25) {
            for p in group {
                println!("  {}", p.display());
            }
            println!();
        }
    }

    let pending = db.journal_pending()?;
    if !pending.is_empty() {
        println!(
            "\n{} interrupted file operation(s) will be finished or rolled back on the next organize/import",
            pending.len()
        );
    }

    let runs = crate::history::list(&coll.logs_dir())?;
    if !runs.is_empty() {
        println!("\nrecent runs:");
        for run in runs.iter().rev().take(5).rev() {
            let state = match &run.finish {
                None => "did not finish",
                Some((_, s)) if s.interrupted => "interrupted",
                Some((_, s)) if s.errors > 0 => "finished with errors",
                Some(_) => "ok",
            };
            let dry = if run.start.dry_run { " (dry run)" } else { "" };
            println!("  {}  {:<9} {state}{dry}", run.id(), run.start.command);
        }
        println!("  (details: `musort log`)");
    }
    println!("\n(as of the last run; the files themselves are the source of truth. Run `musort organize` to refresh.)");
    Ok(())
}
