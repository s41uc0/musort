//! Logging: everything goes to the append-only `.musort/musort.log`; warnings
//! and errors are also shown in the terminal.

use std::fs::OpenOptions;
use std::io::IsTerminal;
use std::path::Path;
use std::sync::Mutex;

use anyhow::{Context, Result};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::filter::EnvFilter;
use tracing_subscriber::fmt::format::Writer;
use tracing_subscriber::fmt::time::ChronoLocal;
use tracing_subscriber::fmt::{FmtContext, FormatEvent, FormatFields};
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::prelude::*;

/// Install logging. With `log_file` None (no collection yet) only the terminal
/// layer is active. `verbose` (the number of `-v`s) sets how much of it the
/// terminal shows: warnings only, then musort's info, then debug.
pub fn init(log_file: Option<&Path>, verbose: u8) -> Result<()> {
    let term_filter = match verbose {
        // lofty's own warnings ("replaced frame ...") are noise in the
        // terminal; they still go to the log file.
        0 => EnvFilter::new("warn,lofty=error"),
        1 => EnvFilter::new("musort=info,warn,lofty=error"),
        _ => EnvFilter::new("musort=debug,warn,lofty=error"),
    };
    let term = tracing_subscriber::fmt::layer()
        .with_writer(crate::ui::term_writer)
        .event_format(TermFormat { ansi: std::io::stderr().is_terminal() })
        .with_filter(term_filter);

    let file = match log_file {
        Some(path) => {
            let f = OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .with_context(|| format!("can't open log file {}", path.display()))?;
            Some(
                tracing_subscriber::fmt::layer()
                    .with_writer(Mutex::new(f))
                    .with_ansi(false)
                    .with_timer(ChronoLocal::new("%Y-%m-%d %H:%M:%S%.3f%:z".into()))
                    .with_target(false)
                    .with_filter(EnvFilter::new("musort=debug,warn")),
            )
        }
        None => None,
    };

    tracing_subscriber::registry().with(term).with(file).init();
    Ok(())
}

/// Terminal format: just the message and its fields, with a `warning:` /
/// `error:` prefix where it matters. (No timestamps or run ids; those are in
/// the log file.)
struct TermFormat {
    ansi: bool,
}

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
        if self.ansi && !color.is_empty() {
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

/// A short, unique-enough id for one invocation, used to tie log lines,
/// journal entries and the `runs` table together.
pub fn new_run_id() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    format!("{:x}-{:x}", now.as_secs(), std::process::id())
}
