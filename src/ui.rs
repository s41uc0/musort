//! Terminal output that cooperates with the status bar.
//!
//! Normally there is a two-line status bar at the bottom (progress, running
//! tallies, and what musort is doing right now), with a line per file that
//! actually changed scrolling above it. In verbose mode (`-v`) there is no
//! status bar: every step is printed on a line of its own, so nothing is
//! overwritten.
//!
//! On a terminal, output uses colors and symbols; piped (or with `NO_COLOR`
//! set) it's plain ASCII with letters instead of symbols.

use std::io::{IsTerminal, Write};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering};
use std::sync::Mutex;

use indicatif::{ProgressBar, ProgressDrawTarget, ProgressState, ProgressStyle};

static BAR: Mutex<Option<ProgressBar>> = Mutex::new(None);
static VERBOSITY: AtomicU8 = AtomicU8::new(0);
static ANSI: AtomicBool = AtomicBool::new(false);
static SUBJECT: Mutex<String> = Mutex::new(String::new());
static TALLY: [AtomicUsize; 8] = [const { AtomicUsize::new(0) }; 8];

pub fn init(verbosity: u8) {
    VERBOSITY.store(verbosity, Ordering::Relaxed);
    let ansi = std::io::stderr().is_terminal() && std::env::var_os("NO_COLOR").is_none_or(|v| v.is_empty());
    ANSI.store(ansi, Ordering::Relaxed);
}

/// For commands that print their output to stdout: style by whether stdout
/// is a terminal instead.
pub fn use_stdout() {
    let ansi = std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none_or(|v| v.is_empty());
    ANSI.store(ansi, Ordering::Relaxed);
}

/// Number of `-v` flags given.
pub fn verbosity() -> u8 {
    VERBOSITY.load(Ordering::Relaxed)
}

pub fn verbose() -> bool {
    verbosity() > 0
}

/// Whether to use colors and symbols.
pub fn ansi() -> bool {
    ANSI.load(Ordering::Relaxed)
}

// ---- styling ----

#[derive(Clone, Copy)]
pub enum Color {
    Green,
    Yellow,
    Red,
    Cyan,
    Dim,
    Bold,
}

pub fn paint(s: impl AsRef<str>, c: Color) -> String {
    let s = s.as_ref();
    if !ansi() || s.is_empty() {
        return s.to_string();
    }
    let code = match c {
        Color::Green => "32",
        Color::Yellow => "33",
        Color::Red => "31",
        Color::Cyan => "36",
        Color::Dim => "2",
        Color::Bold => "1",
    };
    format!("\x1b[{code}m{s}\x1b[0m")
}

/// `fancy` on a terminal, `plain` otherwise.
pub fn sym(fancy: &'static str, plain: &'static str) -> &'static str {
    if ansi() {
        fancy
    } else {
        plain
    }
}

pub fn arrow() -> &'static str {
    sym("→", "->")
}

/// Separator between flags.
pub fn sep() -> String {
    paint(sym(" · ", ", "), Color::Dim)
}

/// What happened to a file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    Move,
    Copy,
    Tag,
    Unidentified,
    Duplicate,
    /// `undo`: a copy removed (its original is intact).
    Remove,
    /// `undo`: left alone because it changed since.
    Skip,
    Error,
}

impl Op {
    const ALL: [Op; 8] =
        [Op::Move, Op::Copy, Op::Tag, Op::Unidentified, Op::Duplicate, Op::Remove, Op::Skip, Op::Error];

    pub fn symbol(self) -> String {
        let (fancy, plain, color) = match self {
            Op::Move => ("→", "M", Color::Green),
            Op::Copy => ("⇉", "C", Color::Green),
            Op::Tag => ("✎", "T", Color::Cyan),
            Op::Unidentified => ("?", "U", Color::Yellow),
            Op::Duplicate => ("≡", "D", Color::Yellow),
            Op::Remove => ("−", "R", Color::Yellow),
            Op::Skip => ("!", "S", Color::Yellow),
            Op::Error => ("✗", "E", Color::Red),
        };
        paint(sym(fancy, plain), color)
    }
}

/// A match source and score, colored by confidence.
pub fn score(source: &str, pct: u8) -> String {
    let color = if pct >= 95 { Color::Green } else { Color::Yellow };
    format!("{} {}", paint(source, Color::Dim), paint(format!("{pct}%"), color))
}

/// The column per-file flags line up at (unless the path is longer).
const FLAGS_COLUMN: usize = 50;

/// One file's outcome: `op origin ... flags`, and the destination on a
/// second line when the file went somewhere.
pub fn format_file_line(op: Op, origin: &str, dest: Option<&str>, flags: &str) -> String {
    let used = 2 + console::measure_text_width(origin);
    let pad = if flags.is_empty() { 0 } else { FLAGS_COLUMN.saturating_sub(used).max(3) };
    let mut s = format!("{} {origin}{}{flags}", op.symbol(), " ".repeat(pad));
    if let Some(dest) = dest {
        s.push_str(&format!("\n  {} {dest}", paint(arrow(), Color::Dim)));
    }
    s
}

/// Report one file's outcome: counted in the status bar's tallies, and
/// printed (unless verbose mode is narrating everything anyway).
pub fn file_line(op: Op, origin: &str, dest: Option<&str>, flags: &str) {
    TALLY[op as usize].fetch_add(1, Ordering::Relaxed);
    if !verbose() {
        line(format_file_line(op, origin, dest, flags));
    }
}

fn tally() -> String {
    Op::ALL
        .iter()
        .filter_map(|&op| {
            let n = TALLY[op as usize].load(Ordering::Relaxed);
            (n > 0).then(|| format!("{} {n}", op.symbol()))
        })
        .collect::<Vec<_>>()
        .join("  ")
}

// ---- output ----

/// Print a line to stderr without garbling an active status bar.
pub fn line(msg: impl AsRef<str>) {
    let msg = msg.as_ref();
    let bar = BAR.lock().unwrap_or_else(|e| e.into_inner());
    match bar.as_ref() {
        Some(pb) if !pb.is_hidden() => pb.println(msg),
        _ => {
            let _ = writeln!(std::io::stderr(), "{msg}");
        }
    }
}

/// A line for the user that also belongs in the log file: logged as plain
/// text, printed styled. (Its log event is kept off the terminal, so verbose
/// mode doesn't show it twice.)
pub fn say(plain: impl AsRef<str>, styled: impl AsRef<str>) {
    tracing::info!(target: crate::log::FILE_ONLY, "{}", plain.as_ref());
    line(styled);
}

/// [`say`] for lines without styling.
pub fn say_plain(msg: impl AsRef<str>) {
    say(msg.as_ref(), msg.as_ref());
}

// ---- status bar ----

/// One line of the status bar per part of musort, each saying what that part
/// is doing right now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lane {
    /// The file being handled: identifying, moving, writing tags, ...
    File,
    MusicBrainz,
    AcoustId,
    /// The scan: the directory being walked.
    Walk,
    /// The scan: the files being read and hashed right now (see [`job`]).
    Jobs,
}

const LANE_COUNT: usize = 5;
const LANE_NAMES: [&str; LANE_COUNT] = ["file", "musicbrainz", "acoustid", "walking", "reading"];

struct LaneState {
    text: String,
    /// A passing detail, such as waiting for a rate limit.
    note: String,
    /// Whether it's about the current file (else it's the last thing that
    /// part did, shown dimmed).
    fresh: bool,
}

static LANES: Mutex<[LaneState; LANE_COUNT]> =
    Mutex::new([const { LaneState { text: String::new(), note: String::new(), fresh: false } }; LANE_COUNT]);
/// The lines the next status bar shows (None: just the file line).
static NEXT_LANES: Mutex<Option<Vec<Lane>>> = Mutex::new(None);
/// Text after the phase name on the first line, computed at each redraw.
type Headline = Box<dyn Fn() -> String + Send>;
static HEADLINE: Mutex<Option<Headline>> = Mutex::new(None);

/// Which lines the next status bar (from [`start`]) shows.
pub fn show_lanes(lanes: &[Lane]) {
    *NEXT_LANES.lock().unwrap_or_else(|e| e.into_inner()) = Some(lanes.to_vec());
}

/// What the first line says after the phase name, recomputed at every redraw
/// (for live counters). Set after [`start`].
pub fn headline(f: impl Fn() -> String + Send + 'static) {
    *HEADLINE.lock().unwrap_or_else(|e| e.into_inner()) = Some(Box::new(f));
}

/// A spinner with just a message, for a phase without per-file progress.
pub fn busy(prefix: &str, msg: impl Into<String>) {
    start(None, prefix);
    status(msg);
}

// ---- jobs: files being worked on in parallel ----

struct Job {
    id: u64,
    name: String,
    stage: &'static str,
    total: u64,
}

static JOBS: Mutex<Vec<Job>> = Mutex::new(Vec::new());
static NEXT_JOB: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// A file being worked on, shown on the `reading` line until dropped.
pub struct JobHandle(u64);

pub fn job(name: impl Into<String>, total_bytes: u64) -> JobHandle {
    let id = NEXT_JOB.fetch_add(1, Ordering::Relaxed);
    let job = Job { id, name: name.into(), stage: "starting", total: total_bytes };
    JOBS.lock().unwrap_or_else(|e| e.into_inner()).push(job);
    JobHandle(id)
}

impl JobHandle {
    fn with(&self, f: impl FnOnce(&mut Job)) {
        if let Some(j) = JOBS.lock().unwrap_or_else(|e| e.into_inner()).iter_mut().find(|j| j.id == self.0) {
            f(j);
        }
    }
    pub fn stage(&self, stage: &'static str) {
        self.with(|j| j.stage = stage);
    }
}

impl Drop for JobHandle {
    fn drop(&mut self) {
        JOBS.lock().unwrap_or_else(|e| e.into_inner()).retain(|j| j.id != self.0);
    }
}

fn render_jobs() -> String {
    let jobs = JOBS.lock().unwrap_or_else(|e| e.into_inner());
    if jobs.is_empty() {
        return String::new();
    }
    let mut parts: Vec<String> = jobs
        .iter()
        .take(3)
        .map(|j| format!("{} ({}, {})", j.name, j.stage, size(j.total)))
        .collect();
    if jobs.len() > 3 {
        parts.push(format!("+{} more", jobs.len() - 3));
    }
    parts.join(sym(" · ", ", "))
}

/// `52 MB`, `1.2 GB`.
pub fn size(bytes: u64) -> String {
    match bytes {
        0..1_000_000 => format!("{} kB", bytes / 1000),
        1_000_000..1_000_000_000 => format!("{} MB", bytes / 1_000_000),
        _ => format!("{:.1} GB", bytes as f64 / 1e9),
    }
}

fn lanes() -> std::sync::MutexGuard<'static, [LaneState; LANE_COUNT]> {
    LANES.lock().unwrap_or_else(|e| e.into_inner())
}

fn redraw() {
    if let Some(pb) = BAR.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
        pb.tick();
    }
}

/// Set what a part is doing now.
pub fn lane(l: Lane, msg: impl Into<String>) {
    {
        let mut g = lanes();
        let s = &mut g[l as usize];
        s.text = msg.into();
        s.note.clear();
        s.fresh = true;
    }
    redraw();
}

/// Add (or with "", clear) a passing detail to a part's line.
pub fn lane_note(l: Lane, note: impl Into<String>) {
    lanes()[l as usize].note = note.into();
    redraw();
}

/// A new file: every line now describes the previous one, until updated.
pub fn new_file() {
    for s in lanes().iter_mut() {
        s.fresh = false;
        s.note.clear();
    }
}

fn render_lane(i: usize, w: &mut dyn std::fmt::Write) {
    let (mut text, fresh) = if i == Lane::Jobs as usize {
        let jobs = render_jobs();
        let fresh = !jobs.is_empty();
        (if fresh { jobs } else { "waiting for the next file".into() }, fresh)
    } else {
        let g = lanes();
        let s = &g[i];
        let mut text = if s.text.is_empty() { "idle".to_string() } else { s.text.clone() };
        if !s.note.is_empty() {
            text = format!("{text}{}{}", sym(" · ", ", "), s.note);
        }
        (text, s.fresh)
    };
    let width = console::Term::stderr().size_checked().map(|(_, w)| w as usize).unwrap_or(100);
    text = console::truncate_str(&text, width.saturating_sub(14), "…").into_owned();
    let text = if fresh { text } else { paint(text, Color::Dim) };
    let _ = write!(w, "{}  {text}", paint(format!("{:>11}", LANE_NAMES[i]), Color::Dim));
}

/// Start a status bar (a spinner if `total` is None). Replaces any previous
/// one and resets the tallies. In verbose mode the bar is hidden (it still
/// counts, but never draws).
pub fn start(total: Option<u64>, prefix: &str) -> ProgressBar {
    for t in &TALLY {
        t.store(0, Ordering::Relaxed);
    }
    for s in lanes().iter_mut() {
        *s = LaneState { text: String::new(), note: String::new(), fresh: false };
    }
    *HEADLINE.lock().unwrap_or_else(|e| e.into_inner()) = None;
    let shown = NEXT_LANES.lock().unwrap_or_else(|e| e.into_inner()).take().unwrap_or_else(|| vec![Lane::File]);
    if verbose() {
        let pb = ProgressBar::with_draw_target(total, ProgressDrawTarget::hidden());
        *BAR.lock().unwrap_or_else(|e| e.into_inner()) = Some(pb.clone());
        return pb;
    }
    let mut template = match total {
        Some(_) => "{prefix:>11}  [{bar:30}] {pos}/{len}  ETA {eta}   {tally}{headline}".to_string(),
        None => "{prefix:>11}  {spinner} {headline}".to_string(),
    };
    for l in &shown {
        template.push_str(&format!("\n{{lane{}}}", *l as usize));
    }
    let mut style = ProgressStyle::with_template(&template)
        .expect("valid template")
        .progress_chars("=> ")
        .with_key("tally", |_: &ProgressState, w: &mut dyn std::fmt::Write| {
            let _ = w.write_str(&tally());
        })
        .with_key("headline", |_: &ProgressState, w: &mut dyn std::fmt::Write| {
            if let Some(f) = HEADLINE.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
                let _ = w.write_str(&f());
            }
        });
    const KEYS: [&str; LANE_COUNT] = ["lane0", "lane1", "lane2", "lane3", "lane4"];
    for (i, key) in KEYS.into_iter().enumerate() {
        style = style.with_key(key, move |_: &ProgressState, w: &mut dyn std::fmt::Write| render_lane(i, w));
    }
    let pb = ProgressBar::with_draw_target(total, ProgressDrawTarget::stderr_with_hz(10)).with_style(style);
    // Keep redrawing even when nothing ticks the bar, so live counters,
    // per-file progress and waits stay current.
    pb.enable_steady_tick(std::time::Duration::from_millis(120));
    pb.set_prefix(prefix.to_string());
    *BAR.lock().unwrap_or_else(|e| e.into_inner()) = Some(pb.clone());
    pb
}

pub fn finish() {
    if let Some(pb) = BAR.lock().unwrap_or_else(|e| e.into_inner()).take() {
        pb.finish_and_clear();
    }
}

/// Set the status bar's file line.
pub fn status(msg: impl Into<String>) {
    lane(Lane::File, msg);
}

/// What's being looked up ("Artist – Title", or the file name), for [`step`].
pub fn subject(s: impl Into<String>) {
    *SUBJECT.lock().unwrap_or_else(|e| e.into_inner()) = s.into();
}

/// A step of the current lookup on a lookup line: `<subject>: <detail>`.
pub fn step(l: Lane, detail: impl std::fmt::Display) {
    let subject = SUBJECT.lock().unwrap_or_else(|e| e.into_inner()).clone();
    lane(l, format!("{subject}: {detail}"));
}

// ---- tracing writer ----

/// `tracing` writer for the terminal layer: routes through [`line`].
pub struct TermWriter(Vec<u8>);

impl Write for TermWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Drop for TermWriter {
    fn drop(&mut self) {
        let s = String::from_utf8_lossy(&self.0);
        let s = s.trim_end();
        if !s.is_empty() {
            line(s);
        }
    }
}

pub fn term_writer() -> TermWriter {
    TermWriter(Vec::new())
}

/// A path for display: relative to the collection root when inside it,
/// otherwise relative to the current directory when that's short
/// (`../dl/x.mp3`), else with `~` for the home directory.
pub fn show_path(root: &std::path::Path, p: &std::path::Path) -> String {
    match p.strip_prefix(root) {
        Ok(rel) => rel.display().to_string(),
        Err(_) => show_outside(p),
    }
}

/// A path relative to the current directory when that's short, else with `~`
/// for the home directory.
pub fn show_outside(p: &std::path::Path) -> String {
    let cwd = std::env::current_dir().and_then(std::fs::canonicalize);
    if let Some(rel) = cwd.ok().and_then(|cwd| relative(&cwd, p)) {
        return rel;
    }
    if let Some(home) = std::env::var_os("HOME") {
        if let Ok(rest) = p.strip_prefix(&home) {
            return format!("~/{}", rest.display());
        }
    }
    p.display().to_string()
}

/// `to` relative to `from`, if they share more than a top-level directory
/// and it takes at most three `..`.
fn relative(from: &std::path::Path, to: &std::path::Path) -> Option<String> {
    let f: Vec<_> = from.components().collect();
    let t: Vec<_> = to.components().collect();
    let common = f.iter().zip(&t).take_while(|(a, b)| a == b).count();
    let ups = f.len() - common;
    if common < 3 || ups > 3 {
        return None;
    }
    let mut out = std::path::PathBuf::new();
    for _ in 0..ups {
        out.push("..");
    }
    for c in &t[common..] {
        out.push(c);
    }
    if out.as_os_str().is_empty() {
        return Some(".".into());
    }
    Some(out.display().to_string())
}

/// `2m13s`, `41s`, `1h02m`.
pub fn duration(d: std::time::Duration) -> String {
    let s = d.as_secs();
    match s {
        0..60 => format!("{}.{}s", s, d.subsec_millis() / 100),
        60..3600 => format!("{}m{:02}s", s / 60, s % 60),
        _ => format!("{}h{:02}m", s / 3600, s / 60 % 60),
    }
}
