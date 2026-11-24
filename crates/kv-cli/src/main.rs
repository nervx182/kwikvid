//! `kwikvid`: the kwikvid command-line interface.
//!
//! Drives the engine headless, the way cloud sessions, CI and scripts use it.
//! Each subcommand lives in its own module with a `run` function; the clap
//! definitions are in [`cli`]. Errors are reported once, here, as
//! `error: <message>` on stderr with exit code 1 (clap's own usage errors exit
//! with 2).

mod analyze;
mod cli;
mod edit;
mod format;
mod spec;

use std::process::ExitCode;

use clap::Parser;

use cli::{Cli, Command};

fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = match cli.command {
        Command::AnalyzeMusic(args) => analyze::run(&args),
        Command::Edit(args) => edit::run(&args),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
}
