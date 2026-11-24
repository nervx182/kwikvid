//! `kwikvid`: the kwikvid command-line interface.
//!
//! Drives the engine headless, the way cloud sessions, CI and scripts use it.
//! Each subcommand lives in its own module with a `run` function; the clap
//! definitions are in [`cli`]. Errors are reported once, here, as
//! `error: <message>` on stderr with exit code 1 (clap's own usage errors exit
//! with 2, and so does refusing to replace an existing `--out` without
//! `--overwrite`). Set `RUST_LOG=debug` for diagnostics on stderr.
//!
//! # Example
//!
//! Render a 30 s beat-synced edit of a folder of GoPro clips to a song:
//!
//! ```sh
//! kwikvid edit --clips ~/GoPro/2024-07-14 --music song.mp3 \
//!     --length 30s --size 1920x1080 --fps 30 --out holiday.mp4
//! ```
//!
//! Clips in a directory are used in recording order (`.mp4`/`.mov`/`.m4v`,
//! any case; GoPro `.LRV` proxies and `.THM` thumbnails are skipped), and
//! GoPro HiLight tags steer the edit. Progress goes to stderr (`--quiet`
//! silences it), followed by a summary line with the output path, length,
//! encoder and elapsed time. `--dry-run` prints the edit decision list
//! instead (`--json` for machine-readable output), and `--seed`, `--pin` and
//! `--exclude` steer the edit.

mod analyze;
mod cli;
mod edit;
mod format;
mod render;
mod spec;

use std::process::ExitCode;

use clap::Parser;
use tracing_subscriber::EnvFilter;

use cli::{Cli, Command};

fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .init();
    let cli = Cli::parse();
    let result = match cli.command {
        Command::AnalyzeMusic(args) => analyze::run(&args),
        Command::Edit(args) => edit::run(&args),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e:#}");
            if e.is::<render::OutputExists>() {
                ExitCode::from(render::OutputExists::EXIT_CODE)
            } else {
                ExitCode::FAILURE
            }
        }
    }
}
