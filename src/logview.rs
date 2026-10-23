//! `musort log`: read the history files back. Lists runs, shows everything
//! one run changed, or follows one file back through every run that touched it.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::collection::Collection;
use crate::history::{self, Change, Reason, Record, RunLog, State};
use crate::ui::{self, Color, Op};

pub fn show(coll: &Collection, target: Option<&str>, all: bool) -> Result<()> {
    ui::use_stdout();
    let runs = history::list(&coll.logs_dir())?;
    let Some(target) = target else {
        list_runs(coll, &runs, all);
        return Ok(());
    };
    match history::find(&runs, target) {
        Ok(i) => show_run(coll, &runs, i),
        Err(not_a_run) => {
            let path = normalize(target)?;
            if path.exists() || target.contains('/') || crate::tags::is_audio(&path) {
                file_history(coll, &runs, &path);
                Ok(())
            } else {
                Err(not_a_run)
            }
        }
    }
}

/// run id -> the id of the (real) undo run that undid it.
fn undone_by(runs: &[RunLog]) -> HashMap<&str, &str> {
    runs.iter()
        .filter(|r| !r.start.dry_run)
        .filter_map(|r| r.start.undoes.as_deref().map(|u| (u, r.id())))
        .collect()
}

fn list_runs(coll: &Collection, runs: &[RunLog], all: bool) {
    if runs.is_empty() {
        println!("no runs yet");
        return;
    }
    let undone = undone_by(runs);
    let skip = if all { 0 } else { runs.len().saturating_sub(20) };
    if skip > 0 {
        println!("{}", ui::paint(format!("({skip} older run(s) not shown; --all shows them)"), Color::Dim));
    }
    for r in &runs[skip..] {
        let mut s = format!("{}  {:<8}  {}", ui::paint(r.id(), Color::Bold), r.start.command, outcome(r));
        if let Some(by) = undone.get(r.id()) {
            s.push_str(&ui::paint(format!("  (undone by {by})"), Color::Yellow));
        }
        if r.start.dry_run {
            s = ui::paint(console::strip_ansi_codes(&s), Color::Dim);
        }
        println!("{s}");
    }
    println!(
        "{}",
        ui::paint(
            format!("\ndetails: musort log <run>{0}undo: musort undo <run>{0}logs: {1}", ui::sym(" · ", ", "), coll.logs_dir().display()),
            Color::Dim
        )
    );
}

/// One-line outcome of a run.
fn outcome(r: &RunLog) -> String {
    let Some((_, s)) = &r.finish else {
        return ui::paint("did not finish (crashed or killed)", Color::Red);
    };
    let mut parts = vec![];
    let mut add = |n: usize, op: Op, word: &str| {
        if n > 0 {
            parts.push(format!("{} {n} {word}", op.symbol()));
        }
    };
    if r.start.command == "undo" {
        add(s.moved, Op::Move, "moved back");
        add(s.tags_written, Op::Tag, "retagged");
        add(s.removed, Op::Remove, "removed");
        add(s.skipped, Op::Skip, "left alone");
    } else {
        add(s.moved, Op::Move, "moved");
        add(s.copied, Op::Copy, "copied");
        add(s.tags_written, Op::Tag, "retagged");
        add(s.unidentified, Op::Unidentified, "unidentified");
        add(s.duplicates + s.duplicates_left_in_place, Op::Duplicate, "duplicate");
    }
    add(s.errors, Op::Error, "error");
    let mut out = format!("{} file(s)", s.files);
    if let Some(u) = &r.start.undoes {
        out = format!("undid {u}");
    }
    if r.start.dry_run {
        out.push_str(" (dry run)");
    }
    if s.interrupted {
        out.push_str(&ui::paint(" interrupted", Color::Yellow));
    }
    if parts.is_empty() {
        parts.push(ui::paint("nothing changed", Color::Dim));
    }
    format!("{out}: {}", parts.join("  "))
}

fn show_path(coll: &Collection, p: &Path) -> String {
    ui::show_path(&coll.root, p)
}

/// An absolute, `..`-free version of a path the user typed (which may not
/// exist anymore).
fn normalize(p: &str) -> Result<PathBuf> {
    let abs = std::path::absolute(p)?;
    if let Ok(c) = std::fs::canonicalize(&abs) {
        return Ok(c);
    }
    let mut out = PathBuf::new();
    for c in abs.components() {
        match c {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::CurDir => {}
            c => out.push(c),
        }
    }
    // The directory may still exist even if the file is gone.
    match (out.parent().and_then(|d| std::fs::canonicalize(d).ok()), out.file_name()) {
        (Some(dir), Some(name)) => Ok(dir.join(name)),
        _ => Ok(out),
    }
}

fn show_run(coll: &Collection, runs: &[RunLog], i: usize) -> Result<()> {
    let r = &runs[i];
    println!("{} {}{}", ui::paint(&r.start.command, Color::Bold), r.id(), if r.start.dry_run { " (dry run)" } else { "" });
    println!("{}", ui::paint(format!("  {}", r.start.argv.join(" ")), Color::Dim));
    let started = chrono::DateTime::parse_from_rfc3339(&r.start.started).ok();
    match &r.finish {
        Some((finished, _)) => {
            let took = chrono::DateTime::parse_from_rfc3339(finished)
                .ok()
                .zip(started)
                .and_then(|(f, s)| (f - s).to_std().ok())
                .map(|d| format!(", took {}", ui::duration(d)))
                .unwrap_or_default();
            println!("  {}{took}", outcome(r));
        }
        None => println!("  {}", outcome(r)),
    }
    if let Some(by) = undone_by(runs).get(r.id()) {
        println!("  {}", ui::paint(format!("undone by {by}"), Color::Yellow));
    }
    println!();
    let items = r.items();
    if items.is_empty() {
        println!("{}", ui::paint("(no files were changed)", Color::Dim));
    }
    for group in items {
        println!("{}", render_item(coll, &group, true));
    }
    println!("{}", ui::paint(format!("\nfull log: {}", r.log_path().display()), Color::Dim));
    Ok(())
}

/// One file's changes in a run, as the run displayed them.
fn render_item(coll: &Collection, group: &[&Change], with_tags: bool) -> String {
    let root = &coll.root;
    let mut op = Op::Tag;
    let mut origin = String::new();
    let mut dest = None;
    let mut flags = vec![];
    let mut tag_lines = vec![];
    for c in group {
        let state_flag = match &c.state {
            State::Done { .. } => None,
            State::Failed(e) => Some(ui::paint(format!("failed: {e}"), Color::Red)),
            State::Unknown => Some(ui::paint("unconfirmed (the run was cut short here)", Color::Yellow)),
        };
        let failed = matches!(c.state, State::Failed(_));
        match &c.record {
            Record::Move { copy, from, to, reason, note, .. } => {
                op = match reason {
                    _ if failed => Op::Error,
                    Reason::Duplicate => Op::Duplicate,
                    Reason::Unidentified => Op::Unidentified,
                    Reason::Undo => Op::Move,
                    Reason::Sorted if *copy => Op::Copy,
                    Reason::Sorted => Op::Move,
                };
                origin = show_path(coll, &from.resolve(root));
                dest = Some(show_path(coll, &to.resolve(root)));
                if *reason == Reason::Undo {
                    flags.push(ui::paint("undo", Color::Dim));
                }
                if !note.is_empty() {
                    flags.push(ui::paint(note, Color::Dim));
                }
            }
            Record::Tags { file, before, after, .. } => {
                if origin.is_empty() {
                    origin = show_path(coll, &file.resolve(root));
                }
                if !failed {
                    flags.insert(0, ui::paint("tagged", Color::Cyan));
                }
                if with_tags {
                    for (field, old, new) in before.diff(after) {
                        tag_lines.push(ui::paint(format!("      {field}: {old} {} {new}", ui::arrow()), Color::Dim));
                    }
                }
            }
            Record::Remove { file, original, .. } => {
                op = if failed { Op::Error } else { Op::Remove };
                origin = show_path(coll, &file.resolve(root));
                flags.push(ui::paint(format!("copy removed; original: {}", show_path(coll, &original.resolve(root))), Color::Dim));
            }
            _ => {}
        }
        flags.extend(state_flag);
    }
    let mut s = ui::format_file_line(op, &origin, dest.as_deref(), &flags.join(&ui::sep()));
    for l in tag_lines {
        s.push('\n');
        s.push_str(&l);
    }
    s
}

/// Follow a file back through history: every run that moved or retagged it.
fn file_history(coll: &Collection, runs: &[RunLog], path: &Path) {
    let root = &coll.root;
    let mut cur: PathBuf = path.to_path_buf();
    let mut events: Vec<(&RunLog, Vec<&Change>)> = vec![];
    for r in runs.iter().rev().filter(|r| !r.start.dry_run) {
        let mut mine = vec![];
        for c in r.changes.iter().rev() {
            if matches!(c.state, State::Failed(_)) {
                continue;
            }
            match &c.record {
                Record::Move { from, to, .. } if to.resolve(root) == cur => {
                    mine.push(c);
                    cur = from.resolve(root);
                }
                Record::Tags { file, .. } | Record::Remove { file, .. } if file.resolve(root) == cur => mine.push(c),
                _ => {}
            }
        }
        if !mine.is_empty() {
            mine.reverse();
            events.push((r, mine));
        }
    }
    if events.is_empty() {
        println!("no history for {} (musort never moved or retagged it, or its logs were deleted)", show_path(coll, path));
        return;
    }
    println!("{}", ui::paint(show_path(coll, path), Color::Bold));
    if cur != path {
        println!("{}", ui::paint(format!("  originally {}", show_path(coll, &cur)), Color::Dim));
    }
    for (r, changes) in events.into_iter().rev() {
        println!("\n{} {}", ui::paint(r.id(), Color::Bold), r.start.command);
        println!("{}", render_item(coll, &changes, true));
    }
}
