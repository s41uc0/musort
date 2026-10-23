//! Logging. Every run that touches the collection gets its own readable log
//! file in `.musort/logs/` with everything musort does, down to debug detail.
//! The terminal shows warnings and errors, plus the narration with `-v`.
//! (The undo history next to it is separate: see `history.rs`.)

use std::fs::OpenOptions;
use std::path::Path;
use std::sync::Mutex;

use anyhow::{Context, Result};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::filter::EnvFilter;
use tracing_subscriber::fmt::format::Writer;
use tracing_subscriber::fmt::{FmtContext, FormatEvent, FormatFields};
use tracing_subscriber::prelude::*;
use tracing_subscriber::registry::LookupSpan;

use crate::ui;

/// Target for events that belong in the log file only, because the terminal
/// already shows them in its own way (per-file lines, styled summaries).
pub const FILE_ONLY: &str = "musort::file_only";

/// Install logging. With `log_file` None (read-only commands) only the
/// terminal layer is active. `verbose` (the number of `-v`s) sets how much the
/// terminal shows: warnings only, then musort's info, then debug.
pub fn init(log_file: Option<&Path>, verbose: u8) -> Result<()> {
    // lofty's own warnings ("replaced frame ...") are noise in the terminal;
    // they still go to the log file.
    let term_filter = match verbose {
        0 => "warn,lofty=error",
        1 => "musort=info,warn,lofty=error",
        _ => "musort=debug,warn,lofty=error",
    };
    let term = tracing_subscriber::fmt::layer()
        .with_writer(ui::term_writer)
        .with_ansi(ui::ansi())
        .event_format(TermFormat)
        .with_filter(EnvFilter::new(format!("{term_filter},{FILE_ONLY}=off")));

    let file = match log_file {
        Some(path) => {
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir).with_context(|| format!("can't create {}", dir.display()))?;
            }
            let f = OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .with_context(|| format!("can't open log file {}", path.display()))?;
            Some(
                tracing_subscriber::fmt::layer()
                    .with_writer(Mutex::new(f))
                    .with_ansi(false)
                    .event_format(FileFormat)
                    .with_filter(EnvFilter::new("musort=debug,info")),
            )
        }
        None => None,
    };

    tracing_subscriber::registry().with(term).with(file).init();
    Ok(())
}

/// Log file format: `2026-10-23 14:02:11.123  INFO  message key=value`.
struct FileFormat;

impl<S, N> FormatEvent<S, N> for FileFormat
where
    S: Subscriber + for<'a> LookupSpan<'a>,
    N: for<'a> FormatFields<'a> + 'static,
{
    fn format_event(&self, ctx: &FmtContext<'_, S, N>, mut w: Writer<'_>, event: &Event<'_>) -> std::fmt::Result {
        let meta = event.metadata();
        write!(w, "{}  {:<5}  ", chrono::Local::now().format("%Y-%m-%d %H:%M:%S%.3f"), meta.level())?;
        // Other crates' events say where they come from.
        if !meta.target().starts_with("musort") {
            write!(w, "[{}] ", meta.target())?;
        }
        ctx.field_format().format_fields(w.by_ref(), event)?;
        writeln!(w)
    }
}

/// Terminal format: just the message and its fields, with a `warning:` /
/// `error:` prefix where it matters. (No timestamps; those are in the log file.)
struct TermFormat;

impl<S, N> FormatEvent<S, N> for TermFormat
where
    S: Subscriber + for<'a> LookupSpan<'a>,
    N: for<'a> FormatFields<'a> + 'static,
{
    fn format_event(&self, ctx: &FmtContext<'_, S, N>, mut w: Writer<'_>, event: &Event<'_>) -> std::fmt::Result {
        let (prefix, color) = match *event.metadata().level() {
            Level::ERROR => ("error: ", "\x1b[31m"),
            Level::WARN => ("warning: ", "\x1b[33m"),
            Level::DEBUG | Level::TRACE => ("  ", "\x1b[2m"),
            Level::INFO => ("", ""),
        };
        if ui::ansi() && !color.is_empty() {
            write!(w, "{color}{prefix}")?;
            ctx.field_format().format_fields(w.by_ref(), event)?;
            writeln!(w, "\x1b[0m")
        } else {
            write!(w, "{prefix}")?;
            ctx.field_format().format_fields(w.by_ref(), event)?;
            writeln!(w)
        }
    }
}
