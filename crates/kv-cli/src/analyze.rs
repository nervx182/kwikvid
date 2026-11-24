//! `kwikvid analyze-music`: tempo, beat count and first beats of a song.

use std::fmt::Write as _;

use anyhow::Context;
use kv_core::Micros;
use serde::Serialize;

use crate::cli::AnalyzeMusicArgs;
use crate::format::{print_json, print_text, round_bpm, secs};

/// JSON output of `analyze-music --json`. Times are microseconds.
#[derive(Debug, Serialize)]
struct Report {
    path: String,
    duration: Micros,
    tempo_bpm: f64,
    beat_count: usize,
    downbeat_count: usize,
    onset_count: usize,
    first_beats: Vec<Micros>,
}

pub fn run(args: &AnalyzeMusicArgs) -> anyhow::Result<()> {
    let path = args.file.display().to_string();
    let a =
        kv_audio::analyze(&args.file).with_context(|| format!("cannot analyse music {path:?}"))?;
    let report = Report {
        path,
        duration: a.duration,
        tempo_bpm: round_bpm(a.tempo_bpm),
        beat_count: a.beats.len(),
        downbeat_count: a.downbeats.len(),
        onset_count: a.onsets.len(),
        first_beats: a.beats.iter().take(args.first).copied().collect(),
    };
    if args.json {
        return print_json(&report);
    }
    let mut s = String::new();
    writeln!(s, "{}", report.path)?;
    writeln!(s, "  duration  {} s", secs(report.duration))?;
    writeln!(s, "  tempo     {:.2} BPM", report.tempo_bpm)?;
    writeln!(
        s,
        "  beats     {} ({} downbeats)",
        report.beat_count, report.downbeat_count
    )?;
    writeln!(s, "  onsets    {}", report.onset_count)?;
    if !report.first_beats.is_empty() {
        let beats: Vec<String> = report.first_beats.iter().map(|&b| secs(b)).collect();
        writeln!(s, "  first beats (s): {}", beats.join(" "))?;
    }
    print_text(&s)
}
