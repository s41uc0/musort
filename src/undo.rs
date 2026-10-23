//! `musort undo <run>`: revert a run using its history file.
//!
//! Files are handled newest first. Before touching a file, undo checks that
//! it is exactly as the run left it (by content hash, or by its tags when the
//! run crashed mid-write); a file that changed since (retagged elsewhere,
//! moved by a later run, ...) is left alone and reported. Moving back uses
//! the same safe moves as everything else, and the undo is itself a run with
//! its own history, so it can be undone too.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use anyhow::{bail, Result};
use tracing::info;

use crate::collection::Collection;
use crate::db::Db;
use crate::fsops::{self, Mode};
use crate::guard::{self, Critical};
use crate::hash::{hash_file, quick_hash};
use crate::history::{Change, History, Reason, Record, RunLog, State};
use crate::model::TrackMeta;
use crate::pipeline::{report_error, Stats};
use crate::tags;
use crate::ui::{self, Color, Op};

pub fn undo(coll: &Collection, db: &Db, run_id: &str, hist: &History, target: &RunLog, dry_run: bool) -> Result<Stats> {
    let mut stats = Stats::default();
    if target.start.dry_run {
        ui::say_plain(format!("{} was a dry run; there is nothing to undo", target.id()));
        return Ok(stats);
    }
    let items: Vec<Vec<&Change>> = target
        .items()
        .into_iter()
        .filter(|g| g.iter().any(|c| !matches!(c.state, State::Failed(_))))
        .collect();
    if items.is_empty() {
        ui::say_plain(format!("{} didn't change anything; there is nothing to undo", target.id()));
        return Ok(stats);
    }
    let what = match &target.start.undoes {
        Some(u) => format!("{} (which undid {u})", target.id()),
        None => format!("{} ({})", target.id(), target.start.command),
    };
    ui::say_plain(format!("undoing {what}: {} file(s)", items.len()));

    let mut u = Undo { coll, db, run_id, hist, dry_run, stats: &mut stats, vacated: BTreeSet::new(), item: 0 };
    let total = items.len();
    let pb = ui::start(Some(total as u64), if dry_run { "planning" } else { "undoing" });
    // Newest first, so a file that was moved twice goes back step by step.
    for (i, group) in items.iter().rev().enumerate() {
        if guard::stop_requested() {
            u.stats.interrupted = true;
            break;
        }
        u.item = i + 1;
        let shown = u.show(&final_path(coll, group));
        info!("[{}/{total}] {shown}", i + 1);
        ui::status(format!("{shown}: checking it's as the run left it"));
        if let Err(e) = u.item(group) {
            u.stats.errors += 1;
            report_error(&shown, &e);
        }
        u.stats.handled += 1;
        pb.inc(1);
    }
    ui::finish();
    if !dry_run {
        for dir in u.vacated.iter().rev() {
            fsops::prune_empty_dirs(dir, &coll.root);
        }
    }
    u.stats.files = total;
    if guard::stop_requested() {
        u.stats.interrupted = true;
    }
    Ok(stats)
}

struct Undo<'a> {
    coll: &'a Collection,
    db: &'a Db,
    run_id: &'a str,
    hist: &'a History,
    dry_run: bool,
    stats: &'a mut Stats,
    vacated: BTreeSet<PathBuf>,
    item: usize,
}

/// Where a file ended up after the run.
fn final_path(coll: &Collection, group: &[&Change]) -> PathBuf {
    group
        .iter()
        .rev()
        .find_map(|c| match &c.record {
            Record::Tags { file, .. } | Record::Remove { file, .. } => Some(file.resolve(&coll.root)),
            Record::Move { to, .. } => Some(to.resolve(&coll.root)),
            _ => None,
        })
        .unwrap_or_default()
}

struct MoveRec {
    copy: bool,
    from: PathBuf,
    to: PathBuf,
    /// What the moved file's content was.
    content: Expect,
    state: State,
}

/// What a file's content should be: a quick fingerprint, or (in histories
/// from before quick fingerprints) a full hash.
#[derive(Debug, Clone)]
enum Expect {
    Quick(String),
    Full(String),
    Unknown,
}

impl Expect {
    fn from(quick: &Option<String>, hash: &Option<String>) -> Self {
        match (quick, hash) {
            (Some(q), _) => Expect::Quick(q.clone()),
            (None, Some(h)) => Expect::Full(h.clone()),
            (None, None) => Expect::Unknown,
        }
    }

    /// Whether the file at `path` has this content (Unknown: can't tell, so yes).
    fn matches(&self, path: &Path) -> Result<bool> {
        Ok(match self {
            Expect::Quick(q) => quick_hash(path, false)?.quick == *q,
            Expect::Full(h) => hash_file(path, false)? == *h,
            Expect::Unknown => true,
        })
    }
}

struct TagsRec {
    file: PathBuf,
    before: TrackMeta,
    after: TrackMeta,
    state: State,
}

impl Undo<'_> {
    fn show(&self, p: &Path) -> String {
        ui::show_path(&self.coll.root, p)
    }

    /// Leave a file alone, and say why.
    fn skip(&mut self, file: &Path, why: String) {
        info!("  left alone: {why}");
        self.stats.skipped += 1;
        ui::file_line(Op::Skip, &self.show(file), None, &ui::paint(why, Color::Yellow));
    }

    fn item(&mut self, group: &[&Change]) -> Result<()> {
        let root = &self.coll.root;
        let mut mv = None;
        let mut tg = None;
        for c in group {
            if matches!(c.state, State::Failed(_)) {
                continue; // never happened
            }
            match &c.record {
                Record::Move { copy, from, to, quick, hash, .. } => {
                    mv = Some(MoveRec {
                        copy: *copy,
                        from: from.resolve(root),
                        to: to.resolve(root),
                        content: Expect::from(quick, hash),
                        state: c.state.clone(),
                    })
                }
                Record::Tags { file, before, after, .. } => {
                    tg = Some(TagsRec {
                        file: file.resolve(root),
                        before: before.clone(),
                        after: after.clone(),
                        state: c.state.clone(),
                    })
                }
                Record::Remove { file, original, .. } => {
                    let why = format!(
                        "that undo removed this copy (the original is {}); import it again instead",
                        self.show(&original.resolve(root))
                    );
                    self.skip(&file.resolve(root), why);
                    return Ok(());
                }
                _ => {}
            }
        }

        // Did each change happen? (Unconfirmed ones: the run crashed right
        // then, so look at the disk.)
        let moved = mv.as_ref().is_some_and(|m| match m.state {
            State::Unknown => m.to.exists() && (m.copy || !m.from.exists()),
            _ => true,
        });
        let file = match (&tg, &mv) {
            (Some(t), _) => t.file.clone(),
            (None, Some(m)) => m.to.clone(),
            (None, None) => return Ok(()),
        };
        if !file.exists() {
            match &mv {
                Some(m) if m.from.exists() && !m.copy => {
                    info!("  already back at {}", self.show(&m.from));
                    self.stats.skipped += 1;
                }
                _ => self.skip(&file, "no longer there (moved or deleted since; undo later runs first?)".into()),
            }
            return Ok(());
        }
        let current = tags::read(&file)?;
        let tagged = match &tg {
            None => false,
            Some(t) => match &t.state {
                State::Done { .. } => true,
                _ if current.meta == t.after => true,
                _ if current.meta == t.before => false,
                _ => {
                    self.skip(&file, "its tags changed since".into());
                    return Ok(());
                }
            },
        };
        if !moved && !tagged {
            info!("  nothing of this file was changed");
            return Ok(());
        }

        // Is it still exactly as the run left it? (Tags written but
        // unconfirmed: their match above is the check.)
        let expected = match (&tg, &mv) {
            (Some(TagsRec { state: State::Done { quick, hash }, .. }), _) if tagged => Expect::from(quick, hash),
            (_, Some(m)) if !tagged => m.content.clone(),
            _ => Expect::Unknown,
        };
        if !expected.matches(&file)? {
            self.skip(&file, "changed since (different content now)".into());
            return Ok(());
        }
        if let Some(m) = mv.as_ref().filter(|m| moved && !m.copy) {
            if m.from.symlink_metadata().is_ok() {
                self.skip(&file, format!("something else is at {} now", self.show(&m.from)));
                return Ok(());
            }
        }

        // A copy-import: the copy goes, if the original is still intact.
        if let Some(m) = mv.as_ref().filter(|m| moved && m.copy) {
            if !m.from.exists() || !m.content.matches(&m.from)? {
                self.skip(&file, format!("the original at {} is gone or changed; keeping this copy", self.show(&m.from)));
                return Ok(());
            }
            let flags = ui::paint(format!("copy removed; the original is {}", self.show(&m.from)), Color::Dim);
            if !self.dry_run {
                let _critical = Critical::enter();
                let quick_now = quick_hash(&file, false)?.quick;
                let seq = self.hist.begin_remove(self.item, &file, &quick_now, &m.from)?;
                let removed = std::fs::remove_file(&file).map_err(anyhow::Error::from).and_then(|()| fsops::fsync_parent(&file));
                if let Err(e) = removed {
                    self.hist.failed(seq, &e)?;
                    return Err(e);
                }
                self.hist.done(seq, None)?;
                info!("  removed the copy {} (the original {} is intact)", self.show(&file), self.show(&m.from));
                self.forget(&file);
            }
            self.stats.removed += 1;
            ui::file_line(Op::Remove, &self.show(&file), None, &flags);
            return Ok(());
        }

        let back = mv.as_ref().filter(|_| moved).map(|m| m.from.clone());
        let mut flags = vec![];
        if tagged {
            flags.push(ui::paint("tags restored", Color::Cyan));
        }
        let flags = flags.join(&ui::sep());
        let op = if back.is_some() { Op::Move } else { Op::Tag };
        let shown = self.show(&file);
        let dest = back.as_ref().map(|b| self.show(b));
        if self.dry_run {
            if let Some(t) = tg.as_ref().filter(|_| tagged) {
                let mut s = ui::format_file_line(op, &shown, dest.as_deref(), &flags);
                for (field, old, new) in t.after.diff(&t.before) {
                    s.push_str(&ui::paint(format!("\n      {field}: {old} {} {new}", ui::arrow()), Color::Dim));
                }
                ui::line(s);
            } else {
                ui::line(ui::format_file_line(op, &shown, dest.as_deref(), &flags));
            }
            self.count(tagged, back.is_some());
            return Ok(());
        }

        let _critical = Critical::enter();
        let mut quick_now = quick_hash(&file, false)?.quick;
        if let Some(t) = tg.as_ref().filter(|_| tagged) {
            if !current.writable {
                bail!("can't write tags to {} files", current.format);
            }
            ui::status(format!("{shown}: restoring the old tags"));
            let seq = self.hist.begin_tags(self.item, &file, &current.meta, &t.before)?;
            match tags::restore(&file, &t.before) {
                Ok(changes) => {
                    for (field, old, new) in &changes {
                        info!("  tag {field}: {old} -> {new}");
                    }
                    quick_now = quick_hash(&file, false)?.quick;
                    self.hist.done(seq, Some(&quick_now))?;
                }
                Err(e) => {
                    self.hist.failed(seq, &e)?;
                    return Err(e);
                }
            }
        }
        if let Some(back) = &back {
            ui::status(format!("{shown}: moving back to {}", self.show(back)));
            let seq = self.hist.begin_move(self.item, false, &file, back, &quick_now, Reason::Undo, "")?;
            if let Err(e) = fsops::transfer(self.db, self.run_id, &file, back, Mode::Move, None) {
                self.hist.failed(seq, &e)?;
                return Err(e);
            }
            self.hist.done(seq, None)?;
            info!("  moved back to {}", self.show(back));
            if let Some(parent) = file.parent() {
                self.vacated.insert(parent.to_path_buf());
            }
        }
        // The database row describes the file as the run left it; the next
        // organize reads it fresh.
        self.forget(&file);
        self.count(tagged, back.is_some());
        ui::file_line(op, &shown, dest.as_deref(), &flags);
        Ok(())
    }

    fn forget(&self, file: &Path) {
        if let Some(rel) = self.coll.rel(file) {
            self.db.delete_track(rel);
        }
    }

    fn count(&mut self, tagged: bool, moved: bool) {
        if tagged {
            self.stats.tags_written += 1;
        }
        if moved {
            self.stats.moved += 1;
        }
    }
}

/// The end-of-run summary of `undo`, as (plain, styled) lines.
pub fn summary(s: &Stats, undone: &str, dry_run: bool) -> Vec<(String, String)> {
    let mut out = vec![];
    let mut add = |sym: String, plain: String| {
        let styled = format!("{sym} {plain}");
        out.push((plain, styled));
    };
    let would = if dry_run { "would be " } else { "" };
    let mut what = vec![format!("{} file(s) {would}moved back", s.moved)];
    if s.tags_written > 0 {
        what.push(format!("{} {would}retagged", s.tags_written));
    }
    if s.removed > 0 {
        what.push(format!("{} cop(ies) {would}removed", s.removed));
    }
    let ok = s.errors == 0 && s.skipped == 0 && !s.interrupted;
    let head = if ok { ui::paint(ui::sym("✓", "ok"), Color::Green) } else { ui::paint("!", Color::Yellow) };
    add(head, format!("{} {undone}: {}", if dry_run { "undo of" } else { "undid" }, what.join(", ")));
    if s.skipped > 0 {
        add(Op::Skip.symbol(), format!("{} file(s) left alone (changed since, or already undone; see above)", s.skipped));
    }
    if s.errors > 0 {
        add(Op::Error.symbol(), format!("{} error(s)", s.errors));
    }
    if s.interrupted {
        add(ui::paint("!", Color::Yellow), "interrupted; run the same undo again to finish it".into());
    }
    out
}
