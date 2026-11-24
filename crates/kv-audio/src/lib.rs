//! kwikvid music analysis: decode a user-supplied music file and find its
//! tempo, beats, downbeats, onsets and energy, so the auto-edit can cut to the
//! music.
//!
//! This is our own implementation (aubio and Essentia are GPL/AGPL and not
//! allowed, see ADR 0005):
//!
//! 1. Decode with `symphonia` (WAV, MP3, AAC/M4A, FLAC), mix down to mono and
//!    resample to [`ANALYSIS_SAMPLE_RATE`] with `rubato`.
//! 2. Onset strength by spectral flux of a log-compressed STFT (`rustfft`).
//! 3. Tempo by autocorrelation of the onset envelope, globally and over
//!    sliding windows (local tempo).
//! 4. Beats by dynamic programming (Ellis 2007), following the local tempo.
//!
//! [`analyze`] does the file I/O; [`analyze_samples`] is the pure analysis of
//! already decoded mono samples.
//!
//! The beat list is the primary cut-point tier; [`MusicAnalysis::downbeats`]
//! is a coarser tier and [`MusicAnalysis::onsets`] a finer, irregular one.
//!
//! For export, [`render_music_bed`] decodes the same file to stereo PCM at
//! the encoder's rate, trimmed to the edit, with volume and fades.

mod beat;
mod bed;
mod decode;
mod onset;
mod tempo;

use std::path::Path;

use kv_core::Micros;
use serde::{Deserialize, Serialize};

pub use bed::{render_music_bed, MusicBedOptions, PcmAudio, DEFAULT_FADE_OUT};

/// Sample rate the analysis runs at, in Hz. Input is resampled to it.
pub const ANALYSIS_SAMPLE_RATE: u32 = 22_050;
/// Length of one [`MusicAnalysis::energy`] block, in microseconds (100 ms).
pub const ENERGY_BLOCK: Micros = Micros(100_000);
/// Beats per bar assumed for [`MusicAnalysis::downbeats`] (guess: 4/4).
pub const BEATS_PER_BAR: usize = 4;

/// Result of analysing one music file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MusicAnalysis {
    /// Length of the decoded audio.
    pub duration: Micros,
    /// Dominant tempo in beats per minute (0 if no beat was found). For music
    /// whose tempo changes this is the tempo of the most common beat interval;
    /// [`Self::beats`] follow the local tempo.
    pub tempo_bpm: f64,
    /// Beat times, increasing.
    pub beats: Vec<Micros>,
    /// Every [`BEATS_PER_BAR`]th beat starting with the first one (guess: 4/4
    /// time, bar starts on the first beat). A subset of [`Self::beats`].
    pub downbeats: Vec<Micros>,
    /// Detected note onsets, increasing; not necessarily on the beat.
    pub onsets: Vec<Micros>,
    /// RMS level per [`ENERGY_BLOCK`] (100 ms), in linear full-scale units.
    pub energy: Vec<f32>,
}

/// Errors from [`analyze`] and [`render_music_bed`].
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// The file could not be read.
    #[error("cannot read music file: {0}")]
    Io(#[from] std::io::Error),
    /// The container or codec could not be decoded.
    #[error("cannot decode music file: {0}")]
    Decode(#[from] symphonia::core::errors::Error),
    /// The file has no decodable audio track.
    #[error("no audio track found")]
    NoAudioTrack,
    /// The file uses something this decoder does not support.
    #[error("unsupported audio: {0}")]
    Unsupported(&'static str),
    /// Resampling failed.
    #[error("resampling failed: {0}")]
    Resample(String),
    /// [`MusicBedOptions`] are out of range.
    #[error("invalid music bed options: {0}")]
    InvalidOptions(&'static str),
}

/// Decode the music file at `path` and analyse it.
pub fn analyze(path: impl AsRef<Path>) -> Result<MusicAnalysis, Error> {
    let audio = decode::decode_mono(path.as_ref())?;
    let samples = decode::resample(&audio.samples, audio.sample_rate, ANALYSIS_SAMPLE_RATE)?;
    let mut analysis = analyze_samples(&samples, ANALYSIS_SAMPLE_RATE);
    // Report the exact decoded length rather than the resampled one.
    analysis.duration = secs_to_micros(audio.samples.len() as f64 / f64::from(audio.sample_rate));
    Ok(analysis)
}

/// Analyse mono samples at `sample_rate` Hz. Pure: no I/O. For best results
/// pass [`ANALYSIS_SAMPLE_RATE`]; other rates work but shift the STFT
/// resolution.
///
/// # Panics
/// If `sample_rate` is 0.
pub fn analyze_samples(samples: &[f32], sample_rate: u32) -> MusicAnalysis {
    assert!(sample_rate > 0, "sample_rate must be > 0");
    let sr = f64::from(sample_rate);
    let frame_rate = sample_rate as f32 / onset::HOP as f32;
    let frame_secs = onset::HOP as f64 / sr;
    let to_micros = |frames: &[usize]| -> Vec<Micros> {
        frames
            .iter()
            .map(|&f| secs_to_micros(f as f64 * frame_secs))
            .collect()
    };

    let env = onset::normalise(&onset::onset_envelope(samples));

    let onset_frames = onset::pick_onsets(
        &env,
        (0.03 * frame_rate).round() as usize,
        (0.1 * frame_rate).round() as usize,
        0.5,
        (0.03 * frame_rate).round().max(1.0) as usize,
    );

    let (beat_frames, tempo_bpm) = match tempo::estimate_period(&env, frame_rate) {
        Some(global) => {
            let periods = tempo::local_periods(&env, frame_rate, global);
            let beats = beat::track(&env, &periods);
            let bpm = refine_tempo(&beats, global, frame_rate);
            (beats, bpm)
        }
        None => (Vec::new(), 0.0),
    };

    let downbeat_frames: Vec<usize> = beat_frames.iter().copied().step_by(BEATS_PER_BAR).collect();

    MusicAnalysis {
        duration: secs_to_micros(samples.len() as f64 / sr),
        tempo_bpm,
        beats: to_micros(&beat_frames),
        downbeats: to_micros(&downbeat_frames),
        onsets: to_micros(&onset_frames),
        energy: energy(samples, sample_rate),
    }
}

/// Tempo in BPM from the mean of tracked beat intervals within 6% of the
/// global autocorrelation period: precise for steady music, and the dominant
/// tempo when it changes. Falls back to the autocorrelation estimate.
fn refine_tempo(beats: &[usize], global: f32, frame_rate: f32) -> f64 {
    let close: Vec<f64> = beats
        .windows(2)
        .map(|w| (w[1] - w[0]) as f64)
        .filter(|&d| (d / f64::from(global) - 1.0).abs() < 0.06)
        .collect();
    let period = if close.len() >= 2 {
        close.iter().sum::<f64>() / close.len() as f64
    } else {
        f64::from(global)
    };
    60.0 * f64::from(frame_rate) / period
}

/// RMS per 100 ms block; the last block may be shorter.
fn energy(samples: &[f32], sample_rate: u32) -> Vec<f32> {
    let block = (u64::from(sample_rate) * ENERGY_BLOCK.0 as u64 / 1_000_000).max(1) as usize;
    samples
        .chunks(block)
        .map(|c| (c.iter().map(|&s| s * s).sum::<f32>() / c.len() as f32).sqrt())
        .collect()
}

fn secs_to_micros(secs: f64) -> Micros {
    Micros((secs * 1_000_000.0).round() as i64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn silence_has_no_beats() {
        let a = analyze_samples(&vec![0.0; 22_050 * 3], ANALYSIS_SAMPLE_RATE);
        assert_eq!(a.duration, Micros(3_000_000));
        assert_eq!(a.tempo_bpm, 0.0);
        assert!(a.beats.is_empty() && a.downbeats.is_empty() && a.onsets.is_empty());
        assert_eq!(a.energy.len(), 30);
        assert!(a.energy.iter().all(|&e| e == 0.0));
    }

    #[test]
    fn empty_input() {
        let a = analyze_samples(&[], ANALYSIS_SAMPLE_RATE);
        assert_eq!(a.duration, Micros(0));
        assert!(a.beats.is_empty() && a.energy.is_empty());
    }

    #[test]
    fn energy_is_rms_per_100ms() {
        let mut x = vec![0.0f32; 22_050];
        for v in &mut x[..2_205] {
            *v = 0.5;
        }
        let e = energy(&x, 22_050);
        assert_eq!(e.len(), 10);
        assert!((e[0] - 0.5).abs() < 1e-6);
        assert!(e[1..].iter().all(|&v| v == 0.0));
    }
}
