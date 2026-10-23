mod cli;
mod collection;
mod config;
mod db;
mod fsops;
mod guard;
mod hash;
mod history;
mod identify;
mod import;
mod log;
mod logview;
mod model;
mod organize;
mod pipeline;
mod scan;
mod status;
mod tags;
mod template;
mod ui;
mod undo;

use std::path::Path;
use std::process::ExitCode;
use std::time::Instant;

use anyhow::Result;
use clap::Parser;
use tracing::{debug, error, info, warn};

use cli::{Cli, Command};
use collection::Collection;
use config::Config;
use db::Db;
use fsops::Mode;
use history::History;
use pipeline::Stats;
use ui::Color;

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli) {
        Ok(code) => code,
        Err(e) => {
            ui::finish();
            error!("{e:#}");
            // The terminal log layer only exists once logging is set up.
            if !tracing::dispatcher::has_been_set() {
                eprintln!("{} {e:#}", ui::paint("error:", Color::Red));
            }
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> Result<ExitCode> {
    ui::init(cli.verbose);
    match cli.command {
        Command::Init { path } => {
            let path = match path.or(cli.root) {
                Some(p) => p,
                None => std::env::current_dir()?,
            };
            let coll = Collection::init(&path)?;
            let logs = coll.logs_dir();
            log::init(Some(&logs.join(format!("{}_init.log", history::new_run_id(&logs)))), ui::verbosity())?;
            info!(root = %coll.root.display(), version = env!("CARGO_PKG_VERSION"), "collection created");
            println!("created a musort collection in {}", coll.root.display());
            println!("settings: {}", coll.config_path().display());
            println!("next: set identify.musicbrainz.contact in the settings, then run `musort import <folder>`");
            Ok(ExitCode::SUCCESS)
        }
        Command::Status => {
            let coll = Collection::find(cli.root.as_deref())?;
            log::init(None, ui::verbosity())?;
            let db = Db::open(&coll.db_path())?;
            status::status(&coll, &db)?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Log { target, all } => {
            let coll = Collection::find(cli.root.as_deref())?;
            log::init(None, ui::verbosity())?;
            logview::show(&coll, target.as_deref(), all)?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Organize(args) => {
            let coll = Collection::find(cli.root.as_deref())?;
            with_run(&coll, "organize", args.dry_run, None, |cfg, db, run_id, hist| {
                organize::organize(&coll, cfg, db, run_id, hist, &args)
            })
        }
        Command::Import { src, copy, run } => {
            let coll = Collection::find(cli.root.as_deref())?;
            let mode = if copy { Mode::Copy } else { Mode::Move };
            with_run(&coll, "import", run.dry_run, None, |cfg, db, run_id, hist| {
                import::import(&coll, cfg, db, run_id, hist, &src, mode, &run)
            })
        }
        Command::Undo { run, dry_run } => {
            let coll = Collection::find(cli.root.as_deref())?;
            let runs = history::list(&coll.logs_dir())?;
            let target = runs[history::find(&runs, &run)?].clone();
            let id = target.id().to_string();
            with_run(&coll, "undo", dry_run, Some(id), |_, db, run_id, hist| {
                undo::undo(&coll, db, run_id, hist, &target, dry_run)
            })
        }
    }
}

/// Common setup and bookkeeping for the commands that change things: the
/// lock, this run's log file and history, crash recovery, and the summary.
fn with_run(
    coll: &Collection,
    command: &str,
    dry_run: bool,
    undoes: Option<String>,
    body: impl FnOnce(&Config, &Db, &str, &History) -> Result<Stats>,
) -> Result<ExitCode> {
    let started = Instant::now();
    guard::install()?;
    let _lock = coll.lock()?;

    let logs = coll.logs_dir();
    std::fs::create_dir_all(&logs)?;
    let run_id = history::new_run_id(&logs);
    let stem = format!("{run_id}_{command}");
    let log_path = logs.join(format!("{stem}.log"));
    log::init(Some(&log_path), ui::verbosity())?;
    let argv: Vec<String> = std::env::args().collect();
    info!(target: log::FILE_ONLY, "musort {} {command}{}", env!("CARGO_PKG_VERSION"), if dry_run { " (dry run)" } else { "" });
    info!(target: log::FILE_ONLY, run = %run_id, argv = ?argv, root = %coll.root.display(), "run started");
    let hist = History::create(
        &coll.root,
        &logs.join(format!("{stem}.jsonl")),
        history::Start {
            run: run_id.clone(),
            command: command.to_string(),
            argv,
            version: env!("CARGO_PKG_VERSION").to_string(),
            started: chrono::Local::now().to_rfc3339(),
            dry_run,
            undoes: undoes.clone(),
        },
    )?;

    let cfg = Config::load(&coll.config_path())?;
    let db = Db::open(&coll.db_path())?;
    if dry_run {
        ui::line(ui::paint("dry run: nothing will be changed", Color::Yellow));
        let pending = db.journal_pending()?.len();
        if pending > 0 {
            warn!(pending, "there are interrupted file operations; a real run will resolve them first");
        }
    } else {
        fsops::recover(&db)?;
    }

    let stats = match body(&cfg, &db, &run_id, &hist) {
        Ok(s) => s,
        Err(e) => {
            let _ = hist.finish(&Stats { errors: 1, ..Default::default() });
            return Err(e);
        }
    };
    hist.finish(&stats)?;
    debug!(stats = %serde_json::to_string(&stats)?, "run finished");

    let mut lines = match &undoes {
        Some(id) => undo::summary(&stats, id, dry_run),
        None => pipeline::summary(&stats, &cfg, dry_run),
    };
    let dim = |s: String| (s.clone(), ui::paint(format!("  {s}"), Color::Dim));
    let mut took = format!("took {}", ui::duration(started.elapsed()));
    if stats.lookups > 0 {
        let matched = stats.via_musicbrainz + stats.via_acoustid;
        took.push_str(&format!("{}{} lookup(s), {matched} matched", ui::sym(" · ", ", "), stats.lookups));
    }
    lines.push(dim(took));
    lines.push(dim(format!("log:  {}", show_path(coll, &log_path))));
    if dry_run {
        lines.push(dim("dry run: nothing was changed".into()));
    } else if stats.changed_anything() {
        lines.push(dim(format!("undo: musort undo {run_id}")));
    }
    for (plain, styled) in lines {
        ui::say(plain, styled);
    }

    Ok(if stats.interrupted {
        ExitCode::from(guard::EXIT_INTERRUPTED as u8)
    } else if stats.errors > 0 {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    })
}

/// A collection path as short as it can be while still working from the
/// current directory.
fn show_path(coll: &Collection, p: &Path) -> String {
    let cwd = std::env::current_dir().ok().and_then(|d| std::fs::canonicalize(d).ok());
    match (cwd, coll.rel(p)) {
        (Some(cwd), Some(rel)) if cwd == coll.root => rel.display().to_string(),
        _ => p.display().to_string(),
    }
}
