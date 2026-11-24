//! Command-line definitions (clap derive). One `Args` struct per subcommand;
//! a new subcommand is a new [`Command`] variant plus a module with `run`.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

use crate::spec::{self, ClipDuration, Length, RangeSpec};

/// kwikvid: beat-synced automatic video edits from clips and a song.
#[derive(Debug, Parser)]
#[command(name = "kwikvid", version, about, propagate_version = true)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Analyse a music file: tempo, beats and first beat times.
    AnalyzeMusic(AnalyzeMusicArgs),
    /// Build the automatic edit for clips and a song.
    Edit(EditArgs),
}

#[derive(Debug, Args)]
pub struct AnalyzeMusicArgs {
    /// Music file (WAV, MP3, AAC/M4A or FLAC).
    pub file: PathBuf,
    /// Print JSON instead of text.
    #[arg(long)]
    pub json: bool,
    /// How many of the first beat times to print.
    #[arg(long, value_name = "N", default_value_t = 8)]
    pub first: usize,
}

#[derive(Debug, Args)]
pub struct EditArgs {
    /// Clip files, or directories whose .mp4/.mov/.m4v files are used in name
    /// order. The edit follows this order.
    #[arg(long, value_name = "FILE|DIR", num_args = 1.., required = true)]
    pub clips: Vec<PathBuf>,
    /// Music file (WAV, MP3, AAC/M4A or FLAC).
    #[arg(long, value_name = "FILE")]
    pub music: PathBuf,
    /// Target length, e.g. 30s, 90, 1m30s, 1:30, or "full" for the whole song.
    #[arg(long, value_name = "LENGTH", default_value = "full", value_parser = spec::parse_length)]
    pub length: Length,
    /// Seed for the deterministic auto-edit; other seeds give other edits.
    #[arg(long, value_name = "N", default_value_t = 0)]
    pub seed: u64,
    /// Force a range into the edit: CLIP@START-END in seconds, e.g.
    /// clip.mp4@12.5-15.0. CLIP is the path as given or its file name.
    #[arg(long, value_name = "CLIP@START-END", value_parser = spec::parse_pin)]
    pub pin: Vec<RangeSpec>,
    /// Force a range (CLIP@START-END) or a whole clip (CLIP) out of the edit.
    #[arg(long, value_name = "CLIP[@START-END]", value_parser = spec::parse_exclude)]
    pub exclude: Vec<RangeSpec>,
    /// Clip durations instead of probing the files: CLIP=SECONDS, comma
    /// separated or repeated (mainly for tests).
    #[arg(long, value_name = "CLIP=SECONDS", value_delimiter = ',', value_parser = spec::parse_clip_duration)]
    pub durations: Vec<ClipDuration>,
    /// Print the edit decision list instead of rendering a video.
    #[arg(long)]
    pub dry_run: bool,
    /// Print JSON instead of a table (with --dry-run).
    #[arg(long)]
    pub json: bool,
}
