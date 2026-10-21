//! Terminal output that cooperates with the progress bar.

//!
//! Normally progress is a single self-updating status line. In verbose mode
//! (`-v`) there is no status line: every step is printed on a line of its own,
//! so the output scrolls and nothing is overwritten.

use std::io::Write;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Mutex;

use indicatif::{ProgressBar, ProgressDrawTarget, ProgressStyle};

static BAR: Mutex<Option<ProgressBar>> = Mutex::new(None);
static VERBOSITY: AtomicU8 = AtomicU8::new(0);

pub fn set_verbosity(v: u8) {
    VERBOSITY.store(v, Ordering::Relaxed);
}

/// Number of `-v` flags given.
pub fn verbosity() -> u8 {
    VERBOSITY.load(Ordering::Relaxed)
}

pub fn verbose() -> bool {
    verbosity() > 0
}

/// Print a line to stderr without garbling an active progress bar.
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

/// Start a progress bar (or spinner, if `total` is None). Replaces any previous one.
/// In verbose mode the bar is hidden (it still counts, but never draws).
pub fn start(total: Option<u64>, prefix: &str) -> ProgressBar {
    if verbose() {
        let pb = ProgressBar::with_draw_target(total, ProgressDrawTarget::hidden());
        *BAR.lock().unwrap_or_else(|e| e.into_inner()) = Some(pb.clone());
        return pb;
    }
    let pb = match total {
        Some(n) => {
            let pb = ProgressBar::new(n);
            pb.set_style(
                ProgressStyle::with_template("{prefix:>9} [{bar:30}] {pos}/{len} {wide_msg}")
                    .unwrap()
                    .progress_chars("=> "),
            );
            pb
        }
        None => {
            let pb = ProgressBar::new_spinner();
            pb.set_style(ProgressStyle::with_template("{prefix:>9} {spinner} {pos} {wide_msg}").unwrap());
            pb.enable_steady_tick(std::time::Duration::from_millis(120));
            pb
        }
    };
    pb.set_draw_target(ProgressDrawTarget::stderr_with_hz(10));
    pb.set_prefix(prefix.to_string());
    *BAR.lock().unwrap_or_else(|e| e.into_inner()) = Some(pb.clone());
    pb
}

pub fn finish() {
    if let Some(pb) = BAR.lock().unwrap_or_else(|e| e.into_inner()).take() {
        pb.finish_and_clear();
    }
}

/// A line meant for the user that should also be in the log file. In verbose
/// mode the terminal log layer already prints it, so it isn't printed twice.
pub fn say(msg: impl AsRef<str>) {
    let msg = msg.as_ref();
    tracing::info!("{msg}");
    if !verbose() {
        line(msg);
    }
}

/// A per-file outcome line for the normal display. Verbose mode already
/// narrates every step, so it's skipped there.
pub fn outcome(msg: impl AsRef<str>) {
    if !verbose() {
        line(msg);
    }
}

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
