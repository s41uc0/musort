use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};

#[derive(Parser, Debug)]
#[command(name = "musort", version, about = "An idiot-proof music collection organizer")]
pub struct Cli {
    /// Collection root (default: the nearest directory containing `.musort/`,
    /// searching upward from the current directory)
    #[arg(long, global = true, value_name = "DIR")]
    pub root: Option<PathBuf>,

    /// Show what musort is doing, one line per step (lookups and their
    /// results, ...) instead of a status bar. -vv shows even more (skipped
    /// files, raw queries). Every run's full log is in .musort/logs/ regardless.
    #[arg(short, long, action = clap::ArgAction::Count, global = true)]
    pub verbose: u8,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Turn a directory into a musort collection (creates `.musort/`)
    Init {
        /// Directory to turn into a collection (default: current directory)
        path: Option<PathBuf>,
    },
    /// Identify everything in the collection and move it into the configured layout
    Organize(RunArgs),
    /// Identify the music in SRC and move it into the collection
    Import {
        /// File or directory to import from
        src: PathBuf,
        /// Copy files instead of moving them (the source is left untouched)
        #[arg(long)]
        copy: bool,
        #[command(flatten)]
        run: RunArgs,
    },
    /// Show what the collection looks like as of the last run
    Status,
    /// Show what musort did: recent runs, everything one run changed, or the
    /// history of one file
    Log {
        /// A run (its id, a unique prefix of it, or `last`) or a file path.
        /// Without it, lists recent runs.
        target: Option<String>,
        /// List all runs, not just the recent ones
        #[arg(long)]
        all: bool,
    },
    /// Revert what a run did: move files back and restore their old tags
    Undo {
        /// The run to undo: its id (see `musort log`), a unique prefix, or `last`
        run: String,
        /// Show what would be undone without changing anything
        #[arg(long, short = 'n')]
        dry_run: bool,
    },
}

#[derive(Args, Debug, Clone)]
pub struct RunArgs {
    /// Show what would happen without touching any file, tag or the database
    #[arg(long, short = 'n')]
    pub dry_run: bool,

    /// Re-identify every file with this source, even files that already carry a
    /// `ref:` tag, and even if the source is disabled in the config
    #[arg(long, value_enum, value_name = "SOURCE")]
    pub force: Option<ForceSource>,

    /// Retry lookups for files that were already looked up and not found
    #[arg(long)]
    pub retry: bool,

    /// Don't contact MusicBrainz/AcoustID; organize using existing tags only
    #[arg(long, conflicts_with = "force")]
    pub no_network: bool,

    /// Quick and loose: don't walk the directories, work from what musort
    /// already knows, only checking that each file is still there and
    /// unchanged. Picks up an interrupted run where it stopped. Files added
    /// since aren't noticed; run without -q for that.
    #[arg(long, short = 'q')]
    pub quick: bool,
}

#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForceSource {
    #[value(alias = "mb")]
    Musicbrainz,
    Acoustid,
}
