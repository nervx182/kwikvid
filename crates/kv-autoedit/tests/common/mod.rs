//! Hand-built inputs shared by the integration tests.
#![allow(dead_code)]

use kv_autoedit::{EditInput, MediaSummary, MusicAnalysis};
use kv_core::{Edl, MediaId, Micros, RangeRef};

pub const SEC: i64 = 1_000_000;

/// Music with a steady beat starting at `first`, downbeats every 4 beats and
/// the given energy per 100 ms block (constant 0.3 when `energy` is empty).
pub fn music(bpm: f64, secs: f64, first: i64, energy: Vec<f32>) -> MusicAnalysis {
    let iv = (60.0 / bpm * 1e6).round() as i64;
    let dur = (secs * 1e6) as i64;
    let beats: Vec<Micros> = (0..)
        .map(|i| Micros(first + i * iv))
        .take_while(|b| b.0 < dur)
        .collect();
    let energy = if energy.is_empty() {
        vec![0.3; (secs * 10.0).ceil() as usize]
    } else {
        energy
    };
    MusicAnalysis {
        duration: Micros(dur),
        tempo_bpm: bpm,
        downbeats: beats.iter().step_by(4).copied().collect(),
        beats,
        onsets: Vec::new(),
        energy,
    }
}

pub fn clip(id: u64, secs: f64, hilights: &[f64], scores: Option<Vec<f32>>) -> MediaSummary {
    MediaSummary {
        id: MediaId(id),
        duration: Micros::from_secs_f64(secs),
        hilights: hilights.iter().map(|&h| Micros::from_secs_f64(h)).collect(),
        scores,
    }
}

pub fn range(id: u64, start: f64, end: f64) -> RangeRef {
    RangeRef {
        media_id: MediaId(id),
        start: Micros::from_secs_f64(start),
        end: Micros::from_secs_f64(end),
    }
}

pub fn input(media: Vec<MediaSummary>, music: MusicAnalysis, target: f64, seed: u64) -> EditInput {
    EditInput {
        media,
        music,
        target_length: Micros::from_secs_f64(target),
        pins: Vec::new(),
        excludes: Vec::new(),
        seed,
    }
}

/// The EDL as an aligned table, one segment per row.
pub fn table(edl: &Edl, music: &MusicAnalysis) -> String {
    let f = |m: Micros| format!("{:.3}", m.as_secs_f64());
    let mut out = String::from(
        "  # | media | src_in  | src_out | start   | length | beats | ends on\n\
         ----+-------+---------+---------+---------+--------+-------+---------\n",
    );
    for (i, s) in edl.segments.iter().enumerate() {
        let len = s.timeline_len();
        let end = Micros(s.timeline_start.0 + len.0);
        let beats_in = music
            .beats
            .iter()
            .filter(|b| **b > s.timeline_start && **b <= end)
            .count();
        let kind = if music.downbeats.contains(&end) {
            "downbeat"
        } else if music.beats.contains(&end) {
            "beat"
        } else {
            "-"
        };
        out.push_str(&format!(
            "{:>3} | {:>5} | {:>7} | {:>7} | {:>7} | {:>6} | {:>5} | {}\n",
            i,
            s.media_id.0,
            f(s.src_in),
            f(s.src_out),
            f(s.timeline_start),
            f(len),
            beats_in,
            kind
        ));
    }
    let total = edl
        .segments
        .last()
        .map(|s| Micros(s.timeline_start.0 + s.timeline_len().0))
        .unwrap_or(Micros(0));
    out.push_str(&format!("total {}\n", f(total)));
    out
}
