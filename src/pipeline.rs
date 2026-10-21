//! The shared per-file pipeline of `organize` and `import`:
//! identify -> decide destination -> (critical) move, write tags, record.

use std::collections::{BTreeSet, HashMap};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::Serialize;
use tracing::{debug, error, info};

use crate::cli::RunArgs;
use crate::collection::Collection;
use crate::config::Config;
use crate::db::{Db, Status, TrackRow};
use crate::fsops::{self, Mode};
use crate::guard::{self, Critical};
use crate::hash::hash_file;
use crate::identify::{self, Engine, Outcome, Query};
use crate::model::{FileInfo, TrackMeta};
use crate::scan::Scanned;
use crate::tags;
use crate::template::Template;
use crate::ui;

/// Where the files being processed come from.
pub enum Origin {
    Collection,
    Import { src_root: PathBuf, mode: Mode },
}

#[derive(Debug, Default, Serialize)]
pub struct Stats {
    pub files: usize,
    pub handled: usize,
    pub moved: usize,
    pub already_in_place: usize,
    pub via_musicbrainz: usize,
    pub via_acoustid: usize,
    pub via_tags: usize,
    pub already_identified: usize,
    pub tags_written: usize,
    pub unidentified: usize,
    pub duplicates: usize,
    pub duplicates_left_in_place: usize,
    pub lookup_failures: usize,
    pub errors: usize,
    pub interrupted: bool,
}

pub struct Pipeline<'a> {
    pub coll: &'a Collection,
    pub cfg: &'a Config,
    pub db: &'a Db,
    pub run_id: &'a str,
    pub args: &'a RunArgs,
    pub origin: Origin,
    template: Template,
    engine: Engine,
    /// Content hash -> files in the collection with that content.
    hashes: HashMap<String, Vec<PathBuf>>,
    /// Directories musort moved files out of (pruned when empty at the end).
    vacated: BTreeSet<PathBuf>,
    /// Year per (album artist, album), for tracks that lack one, so an
    /// album whose tracks are tagged inconsistently isn't split over two folders.
    album_years: HashMap<String, u32>,
    pub stats: Stats,
}

/// What we decided to do with one file.
struct Plan {
    dest_rel: PathBuf,
    status: Status,
    identified_by: Option<&'static str>,
    /// Tags to write (only after a lookup match).
    tag_update: Option<TrackMeta>,
    /// Meta the file will have afterwards (what its path is based on).
    target: TrackMeta,
    lookup_failed_for: Option<String>,
    duplicate: bool,
    note: String,
}

impl<'a> Pipeline<'a> {
    pub fn new(
        coll: &'a Collection,
        cfg: &'a Config,
        db: &'a Db,
        run_id: &'a str,
        args: &'a RunArgs,
        origin: Origin,
    ) -> Result<Self> {
        let template = Template::parse(&cfg.layout.template)?;
        let engine = Engine::new(cfg, args.force, args.no_network)?;
        let mut hashes: HashMap<String, Vec<PathBuf>> = HashMap::new();
        for (path, row) in db.all_tracks()? {
            hashes.entry(row.hash).or_default().push(coll.root.join(path));
        }
        Ok(Pipeline {
            coll,
            cfg,
            db,
            run_id,
            args,
            origin,
            template,
            engine,
            hashes,
            vacated: BTreeSet::new(),
            album_years: HashMap::new(),
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
        self.album_years = album_years(&items);
        if self.engine.active() && items.iter().any(|s| self.needs_lookup(s)) {
            ui::say("identifying files online (MusicBrainz allows one request per second, so this can take a while)");
        }
        let total = items.len();
        let pb = ui::start(Some(total as u64), if self.args.dry_run { "planning" } else { "sorting" });
        for (i, item) in items.into_iter().enumerate() {
            if guard::stop_requested() {
                self.stats.interrupted = true;
                break;
            }
            pb.set_message(item.abs.file_name().unwrap_or_default().to_string_lossy().into_owned());
            // An info event (not a ui line) so the log file gets it too; the steps
            // logged for this file below are indented under it.
            info!("[{}/{total}] {}", i + 1, self.origin_rel(&item.abs).display());
            if let Err(e) = self.process(&item) {
                self.stats.errors += 1;
                error!(path = %item.abs.display(), "{e:#}");
            }
            if !self.stats.interrupted {
                self.stats.handled += 1;
            }
            pb.inc(1);
        }
        ui::finish();
        if self.stats.interrupted || guard::stop_requested() {
            self.stats.interrupted = true;
        }
        if !self.args.dry_run {
            self.prune();
        }
        Ok(())
    }

    fn needs_lookup(&self, s: &Scanned) -> bool {
        if self.args.force.is_some() {
            return true;
        }
        if s.info.meta.is_identified() {
            return false;
        }
        let negative = s.row.as_ref().and_then(|r| r.lookup_failed_for.as_deref()) == Some(s.hash.as_str());
        self.args.retry || !negative
    }

    fn process(&mut self, item: &Scanned) -> Result<()> {
        let plan = self.plan(item);
        if guard::stop_requested() {
            // Ctrl+C during a (slow) lookup: don't start moving this file.
            self.stats.interrupted = true;
            return Ok(());
        }
        let plan = plan?;
        let dest_abs = self.coll.root.join(&plan.dest_rel);
        let src_show = self.origin_rel(&item.abs).display().to_string();
        // A file at a numbered variant of its destination ("Title [2].flac", because
        // "Title.flac" is a different recording) is in place too.
        let in_place = dest_abs == item.abs || fsops::is_numbered_variant(&item.abs, &dest_abs);
        let dest_abs = if in_place { item.abs.clone() } else { dest_abs };

        // Copy-imports never send anything to _duplicates: the original stays
        // where it is anyway.
        if plan.duplicate {
            if let Origin::Import { mode: Mode::Copy, .. } = self.origin {
                self.stats.duplicates_left_in_place += 1;
                info!("  {}; not copying it", plan.note);
                ui::outcome(format!("duplicate  {src_show}  (already in the collection, not copied)"));
                return Ok(());
            }
        }

        if self.args.dry_run {
            self.count(&plan, in_place);
            let what = if in_place { "keep".to_string() } else { format!("-> {}", plan.dest_rel.display()) };
            ui::line(format!("{:<13}{src_show}  {what}  [{}]", self.verb(&plan, in_place), plan.note));
            if let Some(upd) = &plan.tag_update {
                for (field, old, new) in item.info.meta.diff(&merged(&item.info.meta, upd)) {
                    ui::line(format!("             would set {field}: {old} -> {new}"));
                }
            }
            return Ok(());
        }

        // ---- everything below runs to completion, Ctrl+C or not ----
        let _critical = Critical::enter();

        let mode = match &self.origin {
            Origin::Collection => Mode::Move,
            Origin::Import { mode, .. } => *mode,
        };
        let mut dest_abs = dest_abs;
        if !in_place {
            dest_abs = fsops::unique_dest(&dest_abs);
            let method = fsops::transfer(self.db, self.run_id, &item.abs, &dest_abs, mode, &item.hash)?;
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

        // Old db row (if the file was in the collection) goes away with the move.
        let old_rel = match self.origin {
            Origin::Collection => self.coll.rel(&item.abs).map(Path::to_path_buf),
            Origin::Import { .. } => None,
        };

        if plan.duplicate {
            if let Some(old) = &old_rel {
                self.db.delete_track(old)?;
            }
            self.count(&plan, in_place);
            info!("  {}: moved to {}", plan.note, self.show(&dest_abs));
            ui::outcome(format!("duplicate  {src_show}  -> {}", dest_rel.display()));
            return Ok(());
        }

        // Write tags (after the move, so a copy-import never modifies the source).
        let mut info_after = item.info.clone();
        let mut tags_written = false;
        if let Some(upd) = &plan.tag_update {
            if item.info.writable {
                match tags::write(&dest_abs, upd) {
                    Ok(changes) => {
                        for (field, old, new) in &changes {
                            info!("  tag {field}: {old} -> {new}");
                        }
                        tags_written = !changes.is_empty();
                        info_after = tags::read(&dest_abs)?;
                    }
                    Err(e) => {
                        // The file is intact (writes are atomic). It's not
                        // marked identified, so the next run tries again.
                        self.stats.errors += 1;
                        error!(path = %dest_abs.display(), "{e:#}");
                    }
                }
            } else {
                info!(
                    "  musort doesn't write tags to {} files; the identification is kept in the database only",
                    item.info.format
                );
                info_after.meta = plan.target.clone();
            }
        }

        let md = std::fs::metadata(&dest_abs)?;
        let hash = if tags_written { hash_file(&dest_abs, false)? } else { item.hash.clone() };
        let row = TrackRow {
            path: dest_rel.clone(),
            size: md.len(),
            mtime_ns: md.mtime() * 1_000_000_000 + md.mtime_nsec(),
            hash: hash.clone(),
            info: info_after,
            status: plan.status,
            identified_by: plan.identified_by.map(str::to_string),
            lookup_failed_for: plan.lookup_failed_for.clone(),
        };
        self.db.put_track(old_rel.as_deref(), &row)?;
        self.hashes.entry(hash).or_default().push(dest_abs.clone());
        if tags_written {
            self.stats.tags_written += 1;
        }
        self.count(&plan, in_place);
        drop(_critical);

        if !in_place {
            ui::outcome(format!("{:<13}{src_show}  -> {}  [{}]", self.verb(&plan, false), dest_rel.display(), plan.note));
        } else if tags_written {
            ui::outcome(format!("{:<13}{src_show}  [{}]", "tagged", plan.note));
        }
        Ok(())
    }

    /// Decide what to do with a file. Read-only (apart from network lookups).
    fn plan(&mut self, item: &Scanned) -> Result<Plan> {
        let meta = &item.info.meta;
        let previous_failure = item.row.as_ref().and_then(|r| r.lookup_failed_for.clone());
        let mut lookup_failed_for = previous_failure.filter(|h| *h == item.hash);
        let mut tag_update = None;
        let mut identified_by: Option<&'static str> = None;
        let mut note = String::new();

        if !self.needs_lookup(item) {
            if let Some(id) = &meta.ref_musicbrainz {
                info!("  already identified (ref:musicbrainz={id}), no lookup needed");
            } else if let Some(id) = &meta.ref_acoustid {
                info!("  already identified (ref:acoustid={id}), no lookup needed");
            } else {
                info!("  not found online last time and unchanged since; skipping the lookup (use --retry)");
            }
        } else if !self.engine.active() {
            info!("  no lookup: {}", if self.args.no_network { "--no-network" } else { "online lookups are disabled or unavailable" });
        }
        if self.needs_lookup(item) && self.engine.active() {
            info!("  current tags: {}", meta.summary());
            let q = Query { path: &item.abs, meta, duration_ms: item.info.duration_ms };
            match self.engine.identify(&q) {
                Outcome::Found(m) => {
                    info!("  identified via {} ({}%): {}", m.source.name(), m.score, m.meta.summary());
                    debug!(found = %serde_json::to_string(&m.meta).unwrap_or_default(), "  match details");
                    identified_by = Some(m.source.name());
                    note = format!("{} {}%", m.source.name(), m.score);
                    lookup_failed_for = None;
                    tag_update = Some(m.meta);
                }
                Outcome::NotFound => {
                    info!("  no confident match online");
                    lookup_failed_for = Some(item.hash.clone());
                }
                Outcome::Failed => {
                    info!("  lookup failed; will retry next run");
                    self.stats.lookup_failures += 1;
                }
            }
        }
        if identified_by.is_none() {
            if meta.ref_musicbrainz.is_some() {
                identified_by = Some("musicbrainz");
                note = "identified earlier".into();
            } else if meta.ref_acoustid.is_some() {
                identified_by = Some("acoustid");
                note = "identified earlier".into();
            } else if meta.is_complete() {
                identified_by = Some("tags");
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
        let mut duplicate = false;
        let others: Vec<PathBuf> = self
            .hashes
            .get(&item.hash)
            .map(|v| v.iter().filter(|p| **p != item.abs && p.exists()).cloned().collect())
            .unwrap_or_default();
        if let Some(other) = others.first() {
            duplicate = true;
            note = format!("exact duplicate of {}", self.show(other));
        } else {
            let dest_abs = self.coll.root.join(&dest_rel);
            if dest_abs != item.abs && dest_abs.exists() && same_recording(&target, &dest_abs) {
                duplicate = true;
                note = format!("same recording as {}", self.show(&dest_abs));
            }
        }
        if duplicate {
            dest_rel = Path::new(&self.cfg.layout.duplicates_dir).join(&origin_rel);
        }

        Ok(Plan {
            dest_rel,
            status,
            identified_by,
            tag_update,
            target,
            lookup_failed_for,
            duplicate,
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

    fn verb(&self, plan: &Plan, in_place: bool) -> &'static str {
        let copy = matches!(self.origin, Origin::Import { mode: Mode::Copy, .. });
        match (plan.duplicate, plan.status, in_place) {
            (true, ..) => "duplicate",
            (_, Status::Unidentified, _) => "unidentified",
            (_, _, true) => "ok",
            _ if copy => "copy",
            _ => "move",
        }
    }

    fn show(&self, abs: &Path) -> String {
        match self.coll.rel(abs) {
            Some(r) => r.display().to_string(),
            None => abs.display().to_string(),
        }
    }

    fn count(&mut self, plan: &Plan, in_place: bool) {
        let s = &mut self.stats;
        if plan.duplicate {
            s.duplicates += 1;
            return;
        }
        if in_place {
            s.already_in_place += 1;
        } else {
            s.moved += 1;
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

    pub fn summary_lines(&self) -> Vec<String> {
        let s = &self.stats;
        let dry = self.args.dry_run;
        let mut out = vec![];
        let moved_word = match (&self.origin, dry) {
            (Origin::Import { mode: Mode::Copy, .. }, false) => "copied in",
            (Origin::Import { mode: Mode::Copy, .. }, true) => "would be copied in",
            (_, false) => "moved",
            (_, true) => "would be moved",
        };
        let of = if s.interrupted { format!("{} of {} file(s) handled", s.handled, s.files) } else { format!("{} file(s)", s.files) };
        out.push(format!("{of}: {} {moved_word}, {} already in place", s.moved, s.already_in_place));
        let mut ident = vec![];
        if s.via_musicbrainz > 0 {
            ident.push(format!("{} via MusicBrainz", s.via_musicbrainz));
        }
        if s.via_acoustid > 0 {
            ident.push(format!("{} via AcoustID", s.via_acoustid));
        }
        if s.via_tags > 0 {
            ident.push(format!("{} by their existing tags", s.via_tags));
        }
        if s.already_identified > 0 {
            ident.push(format!("{} identified earlier", s.already_identified));
        }
        if !ident.is_empty() {
            out.push(format!("identified: {}", ident.join(", ")));
        }
        if s.tags_written > 0 {
            out.push(format!("tags written to {} file(s)", s.tags_written));
        }
        if s.unidentified > 0 {
            out.push(format!(
                "{} file(s) could not be identified -> {}/",
                s.unidentified, self.cfg.layout.unidentified_dir
            ));
        }
        if s.duplicates > 0 {
            out.push(format!("{} duplicate(s) -> {}/ (review them yourself; musort never deletes)", s.duplicates, self.cfg.layout.duplicates_dir));
        }
        if s.duplicates_left_in_place > 0 {
            out.push(format!("{} duplicate(s) already in the collection were not copied", s.duplicates_left_in_place));
        }
        if s.lookup_failures > 0 {
            out.push(format!("{} lookup(s) failed (network?); those files are retried next run", s.lookup_failures));
        }
        if s.errors > 0 {
            out.push(format!("{} error(s); see .musort/musort.log", s.errors));
        }
        if s.interrupted {
            out.push("interrupted: stopped early; run the same command again to continue".into());
        }
        out
    }
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
