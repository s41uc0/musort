mod cli;
mod collection;
mod config;
mod db;
mod fsops;
mod guard;
mod hash;
mod identify;
mod import;
mod log;
mod model;
mod organize;
mod pipeline;
mod scan;
mod status;
mod tags;
mod template;
mod ui;

use std::process::ExitCode;

use anyhow::Result;
use clap::Parser;
use tracing::{debug, error, info, info_span, warn};

use cli::{Cli, Command};
use collection::Collection;
use config::Config;
use db::Db;
use fsops::Mode;

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli) {
        Ok(code) => code,
        Err(e) => {
            ui::finish();
            error!("{e:#}");
            // The terminal log layer only exists once logging is set up.
            if !tracing::dispatcher::has_been_set() {
                eprintln!("error: {e:#}");
            }
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> Result<ExitCode> {
    ui::set_verbosity(cli.verbose);
    match cli.command {
        Command::Init { path } => {
            let path = match path.or(cli.root) {
                Some(p) => p,
                None => std::env::current_dir()?,
            };
            let coll = Collection::init(&path)?;
            log::init(Some(&coll.log_path()), ui::verbosity())?;
            info!(root = %coll.root.display(), "collection created");
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
        Command::Organize(args) => {
            let coll = Collection::find(cli.root.as_deref())?;
            with_run(&coll, "organize", &args, |cfg, db, run_id| {
                organize::organize(&coll, cfg, db, run_id, &args)
            })
        }
        Command::Import { src, copy, run } => {
            let coll = Collection::find(cli.root.as_deref())?;
            let mode = if copy { Mode::Copy } else { Mode::Move };
            with_run(&coll, "import", &run, |cfg, db, run_id| {
                import::import(&coll, cfg, db, run_id, &src, mode, &run)
            })
        }
    }
}

/// Common setup and bookkeeping for the commands that change things.
fn with_run(
    coll: &Collection,
    command: &str,
    args: &cli::RunArgs,
    body: impl FnOnce(&Config, &Db, &str) -> Result<pipeline::Stats>,
) -> Result<ExitCode> {
    log::init(Some(&coll.log_path()), ui::verbosity())?;
    let run_id = log::new_run_id();
    let _span = info_span!("run", id = %run_id).entered();
    let argv: Vec<String> = std::env::args().collect();
    debug!(command, argv = ?argv, root = %coll.root.display(), version = env!("CARGO_PKG_VERSION"), "run started");

    guard::install()?;
    let _lock = coll.lock()?;
    let cfg = Config::load(&coll.config_path())?;
    let db = Db::open(&coll.db_path())?;

    if args.dry_run {
        ui::line("dry run: nothing will be changed");
        let pending = db.journal_pending()?.len();
        if pending > 0 {
            warn!(pending, "there are interrupted file operations; a real run will resolve them first");
        }
    } else {
        db.run_start(&run_id, command)?;
        fsops::recover(&db)?;
    }

    let result = body(&cfg, &db, &run_id);
    let stats = match result {
        Ok(s) => s,
        Err(e) => {
            if !args.dry_run {
                let _ = db.run_finish(&run_id, false, &format!("failed: {e:#}"));
            }
            return Err(e);
        }
    };
    if !args.dry_run {
        db.run_finish(&run_id, stats.interrupted, &serde_json::to_string(&stats)?)?;
    }
    debug!(stats = %serde_json::to_string(&stats)?, "run finished");

    Ok(if stats.interrupted {
        ExitCode::from(guard::EXIT_INTERRUPTED as u8)
    } else if stats.errors > 0 {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    })
}
