//! The shared per-file pipeline of `organize` and `import`:
//! identify -> decide destination -> (critical) move, write tags, record.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use tracing::{debug, error, info};

use crate::cli::RunArgs;
use crate::collection::Collection;
use crate::config::Config;
use crate::db::{Db, Status, TrackRow, Write};
use crate::fsops::{self, Mode};
use crate::guard::{self, Critical};
use crate::hash::{hash_file_with, quick_hash};
use crate::history::{History, Reason};
use crate::identify::{self, Engine, Outcome, Query};
use crate::model::{FileInfo, TrackMeta};
use crate::scan::Scanned;
use crate::tags;
use crate::template::Template;
use crate::ui::{self, Color, Op};

/// Where the files being processed come from.
pub enum Origin {
    Collection,
    Import { src_root: PathBuf, mode: Mode },
}

/// Counters for one run (also stored in its history file).
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Stats {
    pub files: usize,
    pub handled: usize,
    pub moved: usize,
    pub copied: usize,
    pub already_in_place: usize,
    pub lookups: usize,
    pub via_musicbrainz: usize,
    pub via_acoustid: usize,
    pub via_tags: usize,
    pub already_identified: usize,
    pub tags_written: usize,
    pub unidentified: usize,
    /// Unidentified files moved (or copied) into the unidentified folder.
    pub parked: usize,
    pub duplicates: usize,
    pub duplicates_left_in_place: usize,
    pub lookup_failures: usize,
    /// `undo`: copies removed (their originals were intact).
    pub removed: usize,
    /// `undo`: files left alone because they changed since.
    pub skipped: usize,
    pub errors: usize,
    pub interrupted: bool,
}

impl Stats {
    /// Whether anything on disk changed (so there is something to undo).
    pub fn changed_anything(&self) -> bool {
        self.moved + self.copied + self.parked + self.tags_written + self.duplicates + self.removed > 0
    }
}

pub struct Pipeline<'a> {
    pub coll: &'a Collection,
    pub cfg: &'a Config,
    pub db: &'a Db,
    pub run_id: &'a str,
    pub hist: &'a History,
    pub args: &'a RunArgs,
    pub origin: Origin,
    template: Template,
    engine: Engine,
    /// Size -> files in the collection with that size: only these can be
    /// exact duplicates of a file, so only these are ever compared.
    by_size: HashMap<u64, Vec<PathBuf>>,
    /// What's known about files' content, by path (see [`Known`]).
    content: HashMap<PathBuf, Known>,
    /// Files of this run not handled yet.
    pending: HashSet<PathBuf>,
    /// Directories musort moved files out of (pruned when empty at the end).
    vacated: BTreeSet<PathBuf>,
    /// Year per (album artist, album), for tracks that lack one, so an
    /// album whose tracks are tagged inconsistently isn't split over two folders.
    album_years: HashMap<String, u32>,
    /// Number of the file being handled (1-based), to group its history records.
    item: usize,
    pub stats: Stats,
}

/// A file's fingerprints as far as known. `fresh`: the quick fingerprint was
/// taken from the file during this run (else it's from the database, and is
/// re-taken before it decides anything).
#[derive(Debug, Clone, Default)]
struct Known {
    quick: Option<String>,
    hash: Option<String>,
    fresh: bool,
}

/// How a file was identified.
#[derive(Clone, Copy)]
enum How {
    /// Matched online: source (`mb`/`fp`) and score.
    Matched(&'static str, u8),
    /// Carries a `ref:` tag from an earlier run.
    Earlier,
    /// Placed by its own (complete) tags.
    Tags,
    /// Not identified, and why.
    Unknown(&'static str),
}

/// What we decided to do with one file.
struct Plan {
    dest_rel: PathBuf,
    status: Status,
    identified_by: Option<&'static str>,
    how: How,
    /// Tags to write (only after a lookup match).
    tag_update: Option<TrackMeta>,
    /// Meta the file will have afterwards (what its path is based on).
    target: TrackMeta,
    lookup_failed_for: Option<String>,
    /// For duplicates: the file it duplicates, and whether byte-identical.
    duplicate_of: Option<(PathBuf, bool)>,
    /// One-line explanation for the log.
    note: String,
}

impl Plan {
    fn duplicate(&self) -> bool {
        self.duplicate_of.is_some()
    }
}

impl<'a> Pipeline<'a> {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        coll: &'a Collection,
        cfg: &'a Config,
        db: &'a Db,
        run_id: &'a str,
        hist: &'a History,
        args: &'a RunArgs,
        origin: Origin,
        known: &HashMap<PathBuf, TrackRow>,
    ) -> Result<Self> {
        let template = Template::parse(&cfg.layout.template)?;
        let engine = Engine::new(cfg, args.force, args.no_network)?;
        let mut by_size: HashMap<u64, Vec<PathBuf>> = HashMap::new();
        let mut content = HashMap::new();
        for (abs, row) in known {
            by_size.entry(row.size).or_default().push(abs.clone());
            content.insert(abs.clone(), Known { quick: row.quick.clone(), hash: row.hash.clone(), fresh: false });
        }
        Ok(Pipeline {
            coll,
            cfg,
            db,
            run_id,
            hist,
            args,
            origin,
            template,
            engine,
            by_size,
            content,
            pending: HashSet::new(),
            vacated: BTreeSet::new(),
            album_years: HashMap::new(),
            item: 0,
            stats: Stats::default(),
        })
    }

    pub fn run(&mut self, mut items: Vec<Scanned>) -> Result<()> {
        // Files that were placed before go first, so when two copies of the
        // same file exist, the one already in place is kept as the original.
        items.sort_by_key(|s| {
            let placed = s.row.as_ref().is_some_and(|r| r.status == Status::Organized);
            (!placed, s.abs.clone())
        });
        self.stats.files = items.len();
        if self.album_years.is_empty() {
            self.album_years = album_years(&items);
        }
        // Files in the collection were just fingerprinted: that beats what
        // the database remembers. (Import sources aren't in the collection,
        // so they're nobody's duplicate candidates until they're placed.)
        if matches!(self.origin, Origin::Collection) {
            for s in &items {
                self.know(&s.abs, s.size, Known { quick: Some(s.quick.clone()), hash: s.hash.clone(), fresh: true });
            }
        }
        self.pending = items.iter().map(|s| s.abs.clone()).collect();
        // For the tests: behave as if Ctrl+C was pressed after this many files.
        let stop_after: Option<usize> = std::env::var("MUSORT_TEST_STOP_AFTER").ok().and_then(|v| v.parse().ok());
        let to_look_up = items.iter().filter(|s| self.needs_lookup(s)).count();
        if self.engine.active() && to_look_up > 0 {
            let msg = format!("looking up {to_look_up} file(s) online (about one per second, MusicBrainz's limit)");
            ui::say(&msg, paint_dim(&msg));
        }
        let total = items.len();
        let mut lanes = vec![ui::Lane::File];
        if self.engine.uses(identify::Source::MusicBrainz) {
            lanes.push(ui::Lane::MusicBrainz);
        }
        if self.engine.uses(identify::Source::AcoustId) {
            lanes.push(ui::Lane::AcoustId);
        }
        ui::show_lanes(&lanes);
        let pb = ui::start(Some(total as u64), if self.args.dry_run { "planning" } else { "sorting" });
        // The ETA times these kinds separately: a lookup is about a second
        // (MusicBrainz's limit), a file already in place usually just stays,
        // and other files are moved or copied.
        let kind = |p: &Self, s: &Scanned| {
            if p.engine.active() && p.needs_lookup(s) {
                0
            } else if s.row.as_ref().is_some_and(|r| r.status == Status::Organized) {
                1
            } else {
                2
            }
        };
        let mut counts = [0u64; 3];
        for s in &items {
            counts[kind(self, s)] += 1;
        }
        ui::eta_kinds(&[(counts[0], Some(Duration::from_secs(1))), (counts[1], None), (counts[2], None)]);
        for (i, item) in items.into_iter().enumerate() {
            if guard::stop_requested() {
                self.stats.interrupted = true;
                break;
            }
            ui::eta_begin(kind(self, &item));
            self.item = i + 1;
            let name = item.abs.file_name().unwrap_or_default().to_string_lossy().into_owned();
            let origin = self.origin_rel(&item.abs).display().to_string();
            // Files that are skipped get their status after processing (it's quick),
            // so the line doesn't flicker between "name" and "skipped name".
            ui::new_file();
            if self.needs_lookup(&item) && self.engine.active() {
                let m = &item.info.meta;
                ui::subject(match &m.title {
                    Some(t) => format!("{} – {t}", m.artist.as_deref().unwrap_or("?")),
                    None => name.clone(),
                });
                ui::status(format!("{origin}: identifying"));
            }
            // An info event (not a ui line) so the log file gets it too; the steps
            // logged for this file below are indented under it.
            info!("[{}/{total}] {origin}", i + 1);
            self.pending.remove(&item.abs);
            match self.process(&item) {
                Ok(true) => ui::status(format!("{origin}: skipped, already in place")),
                Ok(false) => ui::status(format!("{origin}: done")),
                Err(e) => {
                    self.stats.errors += 1;
                    ui::status(format!("{origin}: failed"));
                    report_error(&origin, &e);
                }
            }
            if !self.stats.interrupted {
                self.stats.handled += 1;
                // Done with it (errors too: retrying needs a full run), so
                // `--quick` doesn't do it again. (If the mark is lost in a
                // crash, `--quick` just finds the file already handled.)
                if !self.args.dry_run {
                    self.db.worklist_done(&item.abs);
                }
            }
            ui::eta_end();
            pb.inc(1);
            if stop_after == Some(self.stats.handled) {
                guard::request_stop();
            }
        }
        if self.stats.interrupted || guard::stop_requested() {
            self.stats.interrupted = true;
        }
        if !self.args.dry_run && !self.vacated.is_empty() {
            ui::status(format!("removing folders that are empty now ({} checked)", self.vacated.len()));
            self.prune();
        }
        ui::finish();
        Ok(())
    }

    /// Remember what's known about a file in the collection.
    fn know(&mut self, abs: &Path, size: u64, k: Known) {
        let same_size = self.by_size.entry(size).or_default();
        if !same_size.iter().any(|p| p == abs) {
            same_size.push(abs.to_path_buf());
        }
        self.content.insert(abs.to_path_buf(), k);
    }

    /// The quick fingerprint of a collection file, taken from the file if it
    /// wasn't this run (a stale one from the database could mislead).
    fn fresh_quick(&mut self, abs: &Path) -> Result<String> {
        let known = self.content.get(abs).cloned().unwrap_or_default();
        if let (true, Some(q)) = (known.fresh, &known.quick) {
            return Ok(q.clone());
        }
        let q = quick_hash(abs, false)?;
        // The full hash stays valid only if the content is what it was for.
        let hash = q.full.clone().or(known.hash.filter(|_| known.quick.as_deref() == Some(q.quick.as_str())));
        self.content.insert(abs.to_path_buf(), Known { quick: Some(q.quick.clone()), hash, fresh: true });
        Ok(q.quick)
    }

    /// The full hash of a file whose quick fingerprint is fresh, computed if
    /// not known yet (and then remembered, in the database too).
    fn full_hash(&mut self, abs: &Path, shown: &str) -> Result<String> {
        if let Some(h) = self.content.get(abs).and_then(|k| k.hash.clone()) {
            return Ok(h);
        }
        let size = std::fs::metadata(abs)?.len().max(1);
        let h = hash_file_with(abs, false, &|done| {
            ui::lane_note(ui::Lane::File, format!("hashing {shown} to rule out an exact duplicate ({}%)", done * 100 / size));
        })?;
        ui::lane_note(ui::Lane::File, "");
        let k = self.content.entry(abs.to_path_buf()).or_default();
        k.hash = Some(h.clone());
        if let (Some(rel), Some(quick)) = (self.coll.rel(abs), k.quick.clone()) {
            self.db.queue().send(Write::FullHash { path: rel.to_path_buf(), quick, hash: h.clone() });
        }
        Ok(h)
    }

    /// An existing file in the collection with exactly the same content as
    /// `item`, if any. Only files of the same size can be; of those, only
    /// ones with the same quick fingerprint get a full hash comparison.
    fn exact_duplicate(&mut self, item: &Scanned) -> Result<Option<PathBuf>> {
        // Files still to be handled this run aren't candidates: of two copies,
        // the one handled first (the one already in place) is the original.
        let candidates: Vec<PathBuf> = self
            .by_size
            .get(&item.size)
            .map(|v| v.iter().filter(|p| **p != item.abs && !self.pending.contains(*p) && p.exists()).cloned().collect())
            .unwrap_or_default();
        if candidates.is_empty() {
            return Ok(None);
        }
        // The item's own fingerprints, so full_hash can find and keep them.
        let mine = self.content.entry(item.abs.clone()).or_default();
        if mine.quick.as_deref() != Some(item.quick.as_str()) {
            *mine = Known { quick: Some(item.quick.clone()), hash: item.hash.clone(), fresh: true };
        }
        for other in candidates {
            let shown = self.show(&other);
            if self.fresh_quick(&other)? != item.quick {
                continue;
            }
            debug!("  same size and quick fingerprint as {shown}; comparing full hashes");
            let me = self.show(&item.abs);
            if self.full_hash(&item.abs, &me)? == self.full_hash(&other, &shown)? {
                return Ok(Some(other));
            }
            debug!("  not the same content after all");
        }
        Ok(None)
    }

    /// The full set of files this run is part of, when only some of them are
    /// handled (`--quick` continuing a run): album years come from all of them.
    pub fn album_context(&mut self, all: &[Scanned]) {
        self.album_years = album_years(all);
    }

    fn needs_lookup(&self, s: &Scanned) -> bool {
        if self.args.force.is_some() {
            return true;
        }
        if s.info.meta.is_identified() {
            return false;
        }
        let negative = s.row.as_ref().and_then(|r| r.lookup_failed_for.as_deref()) == Some(s.quick.as_str());
        self.args.retry || !negative
    }

    /// Returns whether the file was skipped: already in place, nothing changed.
    fn process(&mut self, item: &Scanned) -> Result<bool> {
        let plan = self.plan(item);
        if guard::stop_requested() {
            // Ctrl+C during a (slow) lookup: don't start moving this file.
            self.stats.interrupted = true;
            return Ok(false);
        }
        let plan = plan?;
        let dest_abs = self.coll.root.join(&plan.dest_rel);
        let src_show = self.origin_rel(&item.abs).display().to_string();
        // A file at a numbered variant of its destination ("Title [2].flac", because
        // "Title.flac" is a different recording) is in place too.
        let in_place = dest_abs == item.abs || fsops::is_numbered_variant(&item.abs, &dest_abs);
        let dest_abs = if in_place { item.abs.clone() } else { dest_abs };
        let mode = match &self.origin {
            Origin::Collection => Mode::Move,
            Origin::Import { mode, .. } => *mode,
        };
        let new_meta = plan
            .tag_update
            .as_ref()
            .map(|u| merged(&item.info.meta, u))
            .filter(|m| *m != item.info.meta && item.info.writable);

        // Copy-imports never send anything to _duplicates: the original stays
        // where it is anyway.
        if plan.duplicate() && mode == Mode::Copy {
            self.stats.duplicates_left_in_place += 1;
            info!("  {}; not copying it", plan.note);
            let flags = [self.dup_flag(&plan), paint_dim("already in the collection, not copied")].join(&ui::sep());
            ui::file_line(Op::Duplicate, &src_show, None, &flags);
            return Ok(false);
        }

        if self.args.dry_run {
            self.count(&plan, in_place, mode);
            let tagging = new_meta.is_some();
            let Some(op) = self.op(&plan, in_place, tagging, mode) else {
                debug!("  skipped: already in place, nothing to change ({})", plan.note);
                return Ok(true);
            };
            let dest = (!in_place).then(|| plan.dest_rel.display().to_string());
            let mut s = ui::format_file_line(op, &src_show, dest.as_deref(), &self.flags(&plan, tagging, false));
            if let Some(m) = &new_meta {
                for (field, old, new) in item.info.meta.diff(m) {
                    s.push_str(&paint_dim(format!("\n      {field}: {old} {} {new}", ui::arrow())));
                }
            }
            ui::line(s);
            return Ok(false);
        }

        // ---- everything below runs to completion, Ctrl+C or not ----
        let _critical = Critical::enter();

        let reason = match (plan.duplicate(), plan.status) {
            (true, _) => Reason::Duplicate,
            (_, Status::Unidentified) => Reason::Unidentified,
            _ => Reason::Sorted,
        };
        let mut dest_abs = dest_abs;
        if !in_place {
            dest_abs = fsops::unique_dest(&dest_abs);
            ui::status(format!(
                "{src_show}: {} {}",
                if mode == Mode::Move { "moving to" } else { "copying to" },
                plan.dest_rel.display()
            ));
            let note = console::strip_ansi_codes(&self.flags(&plan, false, false)).into_owned();
            let seq = self.hist.begin_move(self.item, mode == Mode::Copy, &item.abs, &dest_abs, &item.quick, reason, &note)?;
            let known_hash = self.content.get(&item.abs).and_then(|k| k.hash.clone()).or(item.hash.clone());
            let moved = match fsops::transfer(self.db, self.run_id, &item.abs, &dest_abs, mode, known_hash.as_deref()) {
                Ok(m) => m,
                Err(e) => {
                    self.hist.failed(seq, &e)?;
                    return Err(e);
                }
            };
            self.hist.done(seq, None)?;
            // A copy hashed the file on the way; remember that for free.
            if let Some(h) = moved.hash {
                self.content.entry(item.abs.clone()).or_default().hash = Some(h);
            }
            let method = moved.method;
            info!(
                "  {} to {} ({method}; {}; {})",
                if mode == Mode::Move { "moved" } else { "copied" },
                self.show(&dest_abs),
                plan.target.describe(),
                plan.note
            );
            if mode == Mode::Move {
                if let Some(parent) = item.abs.parent() {
                    self.vacated.insert(parent.to_path_buf());
                }
            }
        }
        let dest_rel = self.coll.rel(&dest_abs).unwrap_or(&plan.dest_rel).to_path_buf();
        let dest_show = (!in_place).then(|| dest_rel.display().to_string());

        // Old db row (if the file was in the collection) goes away with the move.
        let old_rel = match self.origin {
            Origin::Collection => self.coll.rel(&item.abs).map(Path::to_path_buf),
            Origin::Import { .. } => None,
        };

        if plan.duplicate() {
            if let Some(old) = &old_rel {
                self.db.delete_track(old);
            }
            self.count(&plan, in_place, mode);
            info!("  {}: moved to {}", plan.note, self.show(&dest_abs));
            ui::file_line(Op::Duplicate, &src_show, dest_show.as_deref(), &self.dup_flag(&plan));
            return Ok(false);
        }

        // Write tags (after the move, so a copy-import never modifies the source).
        let mut info_after = item.info.clone();
        let mut quick = item.quick.clone();
        let mut hash = self.content.get(&item.abs).and_then(|k| k.hash.clone()).or(item.hash.clone());
        let mut tags_written = false;
        let mut tags_failed = false;
        if let Some(wanted) = &new_meta {
            ui::status(format!("{src_show}: writing {} tag(s), then verifying them", item.info.meta.diff(wanted).len()));
            let seq = self.hist.begin_tags(self.item, &dest_abs, &item.info.meta, wanted)?;
            match tags::write(&dest_abs, wanted) {
                Ok(changes) => {
                    for (field, old, new) in &changes {
                        info!("  tag {field}: {old} -> {new}");
                    }
                    tags_written = !changes.is_empty();
                    info_after = tags::read(&dest_abs)?;
                    // New content: a new quick fingerprint (the full hash is
                    // only known again if it came for free).
                    let q = quick_hash(&dest_abs, false)?;
                    quick = q.quick;
                    hash = q.full;
                    self.hist.done(seq, Some(&quick))?;
                }
                Err(e) => {
                    // The file is intact (writes are atomic). It's not
                    // marked identified, so the next run tries again.
                    self.hist.failed(seq, &e)?;
                    self.stats.errors += 1;
                    tags_failed = true;
                    error!(path = %dest_abs.display(), "{e:#}");
                }
            }
        } else if plan.tag_update.is_some() && !item.info.writable {
            info!(
                "  musort doesn't write tags to {} files; the identification is kept in the database only",
                item.info.format
            );
            info_after.meta = plan.target.clone();
        }

        let md = std::fs::metadata(&dest_abs)?;
        let row = TrackRow {
            path: dest_rel.clone(),
            size: md.len(),
            mtime_ns: md.mtime() * 1_000_000_000 + md.mtime_nsec(),
            quick: Some(quick.clone()),
            hash: hash.clone(),
            info: info_after,
            status: plan.status,
            identified_by: plan.identified_by.map(str::to_string),
            lookup_failed_for: plan.lookup_failed_for.clone(),
        };
        // Through the writer queue (rows are a cache; nobody waits for them).
        // Unchanged files keep their row as is.
        if item.row.as_ref() != Some(&row) || old_rel.as_deref() != Some(row.path.as_path()) {
            self.db.put_track(old_rel.clone(), row);
        }
        self.know(&dest_abs, md.len(), Known { quick: Some(quick), hash, fresh: true });
        if tags_written {
            self.stats.tags_written += 1;
        }
        self.count(&plan, in_place, mode);
        drop(_critical);

        let flags = self.flags(&plan, tags_written, tags_failed);
        match self.op(&plan, in_place, tags_written, mode) {
            Some(_) if tags_failed && in_place => ui::file_line(Op::Error, &src_show, None, &flags),
            Some(op) => ui::file_line(op, &src_show, dest_show.as_deref(), &flags),
            None => {
                debug!("  skipped: already in place, nothing to change ({})", plan.note);
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// The symbol for what happened to a file (None: nothing did).
    fn op(&self, plan: &Plan, in_place: bool, tagged: bool, mode: Mode) -> Option<Op> {
        Some(match (plan.duplicate(), plan.status, in_place) {
            (true, ..) => Op::Duplicate,
            (_, _, true) if tagged => Op::Tag,
            (_, _, true) => return None,
            (_, Status::Unidentified, _) => Op::Unidentified,
            _ if mode == Mode::Copy => Op::Copy,
            _ => Op::Move,
        })
    }

    /// `tagged · mb 97%` and friends.
    fn flags(&self, plan: &Plan, tagged: bool, tags_failed: bool) -> String {
        let mut parts = vec![];
        if tagged {
            parts.push(ui::paint("tagged", Color::Cyan));
        }
        if tags_failed {
            parts.push(ui::paint("tag write failed", Color::Red));
        }
        parts.push(match plan.how {
            How::Matched(src, score) => ui::score(src, score),
            How::Earlier => paint_dim("ref"),
            How::Tags => paint_dim("tags"),
            How::Unknown(why) => ui::paint(why, Color::Yellow),
        });
        parts.join(&ui::sep())
    }

    fn dup_flag(&self, plan: &Plan) -> String {
        match &plan.duplicate_of {
            Some((other, true)) => paint_dim(format!("exact copy of {}", self.show(other))),
            Some((other, false)) => paint_dim(format!("same recording as {}", self.show(other))),
            None => String::new(),
        }
    }

    /// Decide what to do with a file. Read-only (apart from network lookups).
    fn plan(&mut self, item: &Scanned) -> Result<Plan> {
        let meta = &item.info.meta;
        let previous_failure = item.row.as_ref().and_then(|r| r.lookup_failed_for.clone());
        let mut lookup_failed_for = previous_failure.filter(|q| *q == item.quick);
        let mut tag_update = None;
        let mut identified_by: Option<&'static str> = None;
        let mut how = How::Unknown("no match");
        let mut note = String::new();

        if !self.needs_lookup(item) {
            if let Some(id) = &meta.ref_musicbrainz {
                info!("  already identified (ref:musicbrainz={id}), no lookup needed");
            } else if let Some(id) = &meta.ref_acoustid {
                info!("  already identified (ref:acoustid={id}), no lookup needed");
            } else {
                info!("  not found online last time and unchanged since; skipping the lookup (use --retry)");
                how = How::Unknown("no match earlier");
            }
        } else if !self.engine.active() {
            info!("  no lookup: {}", if self.args.no_network { "--no-network" } else { "online lookups are disabled or unavailable" });
            how = How::Unknown(if self.args.no_network { "no lookup (--no-network)" } else { "no lookup" });
        }
        if self.needs_lookup(item) && self.engine.active() {
            info!("  current tags: {}", meta.summary());
            self.stats.lookups += 1;
            let q = Query { path: &item.abs, meta, duration_ms: item.info.duration_ms };
            match self.engine.identify(&q) {
                Outcome::Found(m) => {
                    info!("  identified via {} ({}%): {}", m.source.name(), m.score, m.meta.summary());
                    debug!(found = %serde_json::to_string(&m.meta).unwrap_or_default(), "  match details");
                    identified_by = Some(m.source.name());
                    how = How::Matched(if m.source == identify::Source::AcoustId { "fp" } else { "mb" }, m.score);
                    note = format!("{} {}%", m.source.name(), m.score);
                    lookup_failed_for = None;
                    tag_update = Some(m.meta);
                }
                Outcome::NotFound => {
                    info!("  no confident match online");
                    lookup_failed_for = Some(item.quick.clone());
                }
                Outcome::Failed => {
                    info!("  lookup failed; will retry next run");
                    how = How::Unknown("lookup failed");
                    self.stats.lookup_failures += 1;
                }
            }
        }
        if identified_by.is_none() {
            if meta.ref_musicbrainz.is_some() {
                identified_by = Some("musicbrainz");
                how = How::Earlier;
                note = "identified earlier".into();
            } else if meta.ref_acoustid.is_some() {
                identified_by = Some("acoustid");
                how = How::Earlier;
                note = "identified earlier".into();
            } else if meta.is_complete() {
                identified_by = Some("tags");
                how = How::Tags;
                note = "by existing tags".into();
            }
        }

        let target = match &tag_update {
            Some(upd) => merged(meta, upd),
            None => meta.clone(),
        };
        let ext = item.abs.extension().map(|e| e.to_string_lossy().into_owned()).unwrap_or_default();
        let origin_rel = self.origin_rel(&item.abs);
        let (mut dest_rel, status) = match identified_by {
            Some(_) => {
                let mut for_path = target.clone();
                if for_path.year.is_none() {
                    for_path.year = album_key(&target).and_then(|k| self.album_years.get(&k).copied());
                }
                (self.template.render(&for_path, &ext), Status::Organized)
            }
            None => {
                note = "not identified".into();
                let unid = Path::new(&self.cfg.layout.unidentified_dir);
                let dest = if origin_rel.starts_with(unid) && matches!(self.origin, Origin::Collection) {
                    origin_rel.clone() // already parked there
                } else {
                    unid.join(&origin_rel)
                };
                (dest, Status::Unidentified)
            }
        };

        // Duplicate checks: identical content anywhere in the collection, or
        // the very same recording already sitting at the destination.
        let mut duplicate_of = None;
        if let Some(other) = self.exact_duplicate(item)? {
            note = format!("exact duplicate of {}", self.show(&other));
            duplicate_of = Some((other, true));
        } else {
            let dest_abs = self.coll.root.join(&dest_rel);
            if dest_abs != item.abs && dest_abs.exists() && same_recording(&target, &dest_abs) {
                note = format!("same recording as {}", self.show(&dest_abs));
                duplicate_of = Some((dest_abs, false));
            }
        }
        if duplicate_of.is_some() {
            dest_rel = Path::new(&self.cfg.layout.duplicates_dir).join(&origin_rel);
        }

        Ok(Plan {
            dest_rel,
            status,
            identified_by,
            how,
            tag_update,
            target,
            lookup_failed_for,
            duplicate_of,
            note,
        })
    }

    /// Path of a source file relative to where it came from (the collection
    /// root, or for imports `<import dir name>/<path inside it>`).
    fn origin_rel(&self, abs: &Path) -> PathBuf {
        match &self.origin {
            Origin::Collection => self.coll.rel(abs).unwrap_or(abs).to_path_buf(),
            Origin::Import { src_root, .. } => {
                if src_root.is_file() {
                    PathBuf::from(abs.file_name().unwrap_or_default())
                } else {
                    let name = src_root.file_name().map(PathBuf::from).unwrap_or_default();
                    name.join(abs.strip_prefix(src_root).unwrap_or(abs))
                }
            }
        }
    }

    fn show(&self, abs: &Path) -> String {
        match self.coll.rel(abs) {
            Some(r) => r.display().to_string(),
            None => abs.display().to_string(),
        }
    }

    fn count(&mut self, plan: &Plan, in_place: bool, mode: Mode) {
        let s = &mut self.stats;
        if plan.duplicate() {
            s.duplicates += 1;
            return;
        }
        match (in_place, plan.status, mode) {
            (true, ..) => s.already_in_place += 1,
            (false, Status::Unidentified, _) => s.parked += 1,
            (false, _, Mode::Move) => s.moved += 1,
            (false, _, Mode::Copy) => s.copied += 1,
        }
        match (plan.identified_by, plan.tag_update.is_some()) {
            (None, _) => s.unidentified += 1,
            (Some("musicbrainz"), true) => s.via_musicbrainz += 1,
            (Some("acoustid"), true) => s.via_acoustid += 1,
            (Some("tags"), _) => s.via_tags += 1,
            (Some(_), _) => s.already_identified += 1,
        }
    }

    /// Remove directories that became empty because musort moved files out.
    fn prune(&self) {
        let stop_at = match &self.origin {
            Origin::Collection => self.coll.root.clone(),
            Origin::Import { src_root, mode: Mode::Move } if src_root.is_dir() => src_root.clone(),
            Origin::Import { .. } => return,
        };
        // Deepest first, so parents empty out after their children.
        for dir in self.vacated.iter().rev() {
            fsops::prune_empty_dirs(dir, &stop_at);
        }
    }
}

fn paint_dim(s: impl AsRef<str>) -> String {
    ui::paint(s, Color::Dim)
}

/// A file that couldn't be handled: an error line in the normal display
/// (the full error goes to the log), or the error itself in verbose mode.
pub fn report_error(origin: &str, e: &anyhow::Error) {
    if ui::verbose() {
        error!(file = origin, "{e:#}");
    } else {
        error!(target: crate::log::FILE_ONLY, file = origin, "{e:#}");
        ui::file_line(Op::Error, origin, None, &ui::paint(format!("{e:#}"), Color::Red));
    }
}

/// The end-of-run summary of `organize` / `import`, as (plain, styled) lines.
pub fn summary(s: &Stats, cfg: &Config, dry_run: bool) -> Vec<(String, String)> {
    let mut out = vec![];
    let mut add = |sym: String, plain: String| {
        let styled = format!("{sym} {plain}");
        out.push((plain, styled));
    };
    let would = if dry_run { "would be " } else { "" };
    let files = if s.interrupted { format!("{} of {} file(s)", s.handled, s.files) } else { format!("{} file(s)", s.files) };
    let mut what = vec![];
    if s.moved > 0 || s.copied == 0 {
        what.push(format!("{} {would}moved", s.moved));
    }
    if s.copied > 0 {
        what.push(format!("{} {would}copied in", s.copied));
    }
    if s.tags_written > 0 {
        what.push(format!("{} retagged", s.tags_written));
    }
    if s.already_in_place > 0 {
        what.push(format!("{} already in place", s.already_in_place));
    }
    let ok = s.errors == 0 && !s.interrupted;
    let head = if ok { ui::paint(ui::sym("✓", "ok"), Color::Green) } else { ui::paint("!", Color::Yellow) };
    add(head, format!("{files}: {}", what.join(", ")));
    if s.unidentified > 0 {
        add(
            Op::Unidentified.symbol(),
            format!("{} unidentified {} {}/", s.unidentified, ui::arrow(), cfg.layout.unidentified_dir),
        );
    }
    if s.duplicates > 0 {
        add(
            Op::Duplicate.symbol(),
            format!(
                "{} duplicate(s) {} {}/ (never deleted; review them yourself)",
                s.duplicates,
                ui::arrow(),
                cfg.layout.duplicates_dir
            ),
        );
    }
    if s.duplicates_left_in_place > 0 {
        add(Op::Duplicate.symbol(), format!("{} already in the collection, not copied", s.duplicates_left_in_place));
    }
    if s.lookup_failures > 0 {
        add(
            ui::paint("!", Color::Yellow),
            format!("{} lookup(s) failed (network?); retried next run", s.lookup_failures),
        );
    }
    if s.errors > 0 {
        add(Op::Error.symbol(), format!("{} error(s)", s.errors));
    }
    if s.interrupted {
        add(ui::paint("!", Color::Yellow), "interrupted; run the same command with -q to pick up where it stopped".into());
    }
    out
}

fn album_key(m: &TrackMeta) -> Option<String> {
    let artist = m.album_artist.as_ref().or(m.artist.as_ref())?;
    let album = m.album.as_ref()?;
    Some(format!("{}|{}", identify::norm(artist), identify::norm(album)))
}

/// The most common year per album (see `Pipeline::album_years`).
fn album_years(items: &[Scanned]) -> HashMap<String, u32> {
    let mut counts: HashMap<String, HashMap<u32, usize>> = HashMap::new();
    for s in items {
        if let (Some(k), Some(y)) = (album_key(&s.info.meta), s.info.meta.year) {
            *counts.entry(k).or_default().entry(y).or_default() += 1;
        }
    }
    counts
        .into_iter()
        .filter_map(|(k, years)| years.into_iter().max_by_key(|&(y, n)| (n, std::cmp::Reverse(y))).map(|(y, _)| (k, y)))
        .collect()
}

fn merged(base: &TrackMeta, upd: &TrackMeta) -> TrackMeta {
    let mut m = base.clone();
    tags::overlay(&mut m, upd);
    m
}

/// Is the file at `existing` the same recording as `target`? (Same
/// MusicBrainz recording, or failing that, the same artist/album/track/title.)
fn same_recording(target: &TrackMeta, existing: &Path) -> bool {
    let Ok(FileInfo { meta: other, .. }) = tags::read(existing) else {
        return false;
    };
    if let (Some(a), Some(b)) = (&target.mb_recording_id, &other.mb_recording_id) {
        return a == b;
    }
    let eq = |a: &Option<String>, b: &Option<String>| match (a, b) {
        (Some(a), Some(b)) => identify::norm(a) == identify::norm(b),
        _ => false,
    };
    eq(&target.artist, &other.artist)
        && eq(&target.title, &other.title)
        && eq(&target.album, &other.album)
        && target.track == other.track
}
