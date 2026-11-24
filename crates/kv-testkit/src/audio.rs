//! Synthetic audio fixtures: click tracks with known beat times.
//!
//! A click is a short 1 kHz sine burst with a fast exponential decay, starting
//! at phase zero exactly on the beat sample, so onset detectors see a sharp
//! transient. Everything between clicks is digital silence.

use std::io::Cursor;
use std::path::PathBuf;

use crate::cache::{ensure_fixture, name_num};

/// Default sample rate for audio fixtures, in Hz.
pub const DEFAULT_SAMPLE_RATE: u32 = 44_100;
/// Frequency of the sine burst used for each click, in Hz.
pub const CLICK_FREQUENCY_HZ: f64 = 1_000.0;
/// Length of each click, in seconds (10 ms).
pub const CLICK_DURATION_SECS: f64 = 0.010;
/// Exponential decay time constant of the click envelope, in seconds.
const CLICK_DECAY_SECS: f64 = 0.002;
/// Peak amplitude of a click, as a fraction of full scale.
const CLICK_AMPLITUDE: f64 = 0.9;
/// Bumped whenever the rendered bytes change, so stale cached files are not
/// reused.
const FORMAT_VERSION: u32 = 1;

/// One constant-tempo stretch of a tempo-change track.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TempoSegment {
    /// Tempo in beats per minute (> 0).
    pub bpm: f64,
    /// Duration of the segment in seconds (> 0).
    pub seconds: f64,
}

/// The canonical tempo-change fixture: 100 BPM for 8 s, then 140 BPM for 8 s.
pub const TEMPO_CHANGE_100_140: [TempoSegment; 2] = [
    TempoSegment {
        bpm: 100.0,
        seconds: 8.0,
    },
    TempoSegment {
        bpm: 140.0,
        seconds: 8.0,
    },
];

/// Ground-truth beat times, in seconds, of a [`click_track`] with these
/// parameters: `k * 60 / bpm` for every `k >= 0` with a time strictly below
/// `seconds`. The first beat is at `0.0`.
///
/// # Panics
/// If `bpm` is not finite and positive or `seconds` is not finite and
/// non-negative.
pub fn beat_times(bpm: f64, seconds: f64) -> Vec<f64> {
    check_bpm(bpm);
    assert!(
        seconds.is_finite() && seconds >= 0.0,
        "seconds must be finite and >= 0, got {seconds}"
    );
    beats_in(bpm, seconds, 0.0)
}

/// Ground-truth beat times, in seconds, of a [`tempo_change_track`] with these
/// segments. Each segment's first beat falls exactly on the segment's start
/// time; within a segment beats are `60 / bpm` apart and strictly before the
/// segment's end.
///
/// # Panics
/// If `segments` is empty or any segment is invalid (see [`TempoSegment`]).
pub fn tempo_change_beat_times(segments: &[TempoSegment]) -> Vec<f64> {
    check_segments(segments);
    let mut out = Vec::new();
    let mut start = 0.0;
    for seg in segments {
        out.extend(beats_in(seg.bpm, seg.seconds, start));
        start += seg.seconds;
    }
    out
}

/// Path to a mono 16-bit PCM WAV click track: one click per beat at the times
/// given by [`beat_times`]`(bpm, seconds)`, total length `seconds`. Generated
/// on first use and cached in [`crate::fixtures_dir`].
///
/// # Panics
/// On invalid parameters or if the fixture cannot be written.
pub fn click_track(bpm: f64, seconds: f64, sample_rate: u32) -> PathBuf {
    // Validate before touching the cache so bad parameters always panic.
    beat_times(bpm, seconds);
    check_sample_rate(sample_rate);
    let name = format!(
        "click_v{FORMAT_VERSION}_{}bpm_{}s_{sample_rate}hz.wav",
        name_num(bpm),
        name_num(seconds)
    );
    ensure_fixture(&name, || render_click_track_wav(bpm, seconds, sample_rate))
}

/// Path to a mono 16-bit PCM WAV click track whose tempo changes between
/// segments; beats are at [`tempo_change_beat_times`]`(segments)`. Use
/// [`TEMPO_CHANGE_100_140`] for the canonical fixture. Cached like
/// [`click_track`].
///
/// # Panics
/// On invalid parameters or if the fixture cannot be written.
pub fn tempo_change_track(segments: &[TempoSegment], sample_rate: u32) -> PathBuf {
    check_segments(segments);
    check_sample_rate(sample_rate);
    let parts: Vec<String> = segments
        .iter()
        .map(|s| format!("{}bpm{}s", name_num(s.bpm), name_num(s.seconds)))
        .collect();
    let name = format!(
        "tempo_v{FORMAT_VERSION}_{}_{sample_rate}hz.wav",
        parts.join("-")
    );
    ensure_fixture(&name, || render_tempo_change_wav(segments, sample_rate))
}

/// Render the bytes of the WAV file that [`click_track`] caches, without
/// touching the disk.
pub fn render_click_track_wav(bpm: f64, seconds: f64, sample_rate: u32) -> Vec<u8> {
    let beats = beat_times(bpm, seconds);
    check_sample_rate(sample_rate);
    render_wav(&beats, seconds, sample_rate)
}

/// Render the bytes of the WAV file that [`tempo_change_track`] caches,
/// without touching the disk.
pub fn render_tempo_change_wav(segments: &[TempoSegment], sample_rate: u32) -> Vec<u8> {
    let beats = tempo_change_beat_times(segments);
    check_sample_rate(sample_rate);
    let total: f64 = segments.iter().map(|s| s.seconds).sum();
    render_wav(&beats, total, sample_rate)
}

fn beats_in(bpm: f64, seconds: f64, offset: f64) -> Vec<f64> {
    let period = 60.0 / bpm;
    // Multiply rather than accumulate so times stay exact and drift-free.
    (0u64..)
        .map(|k| k as f64 * period)
        .take_while(|&t| t < seconds)
        .map(|t| offset + t)
        .collect()
}

fn render_wav(beats: &[f64], seconds: f64, sample_rate: u32) -> Vec<u8> {
    let sr = f64::from(sample_rate);
    let total = (seconds * sr).round() as usize;
    let click = click_samples(sample_rate);

    let mut mix = vec![0.0f64; total];
    for &t in beats {
        let start = (t * sr).round() as usize;
        for (dst, &src) in mix.iter_mut().skip(start).zip(&click) {
            *dst += src;
        }
    }

    let spec = hound::WavSpec {
        channels: 1,
        sample_rate,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut buf = Vec::with_capacity(44 + total * 2);
    {
        let mut writer = hound::WavWriter::new(Cursor::new(&mut buf), spec)
            .expect("writing a WAV header to memory cannot fail");
        for x in mix {
            let s = (x * f64::from(i16::MAX))
                .round()
                .clamp(f64::from(i16::MIN), f64::from(i16::MAX)) as i16;
            writer
                .write_sample(s)
                .expect("writing WAV samples to memory cannot fail");
        }
        writer
            .finalize()
            .expect("finalising an in-memory WAV cannot fail");
    }
    buf
}

/// One click as floating-point samples in [-1, 1].
fn click_samples(sample_rate: u32) -> Vec<f64> {
    let sr = f64::from(sample_rate);
    let n = (CLICK_DURATION_SECS * sr).round() as usize;
    (0..n)
        .map(|i| {
            let t = i as f64 / sr;
            CLICK_AMPLITUDE
                * (-t / CLICK_DECAY_SECS).exp()
                * (std::f64::consts::TAU * CLICK_FREQUENCY_HZ * t).sin()
        })
        .collect()
}

fn check_bpm(bpm: f64) {
    assert!(
        bpm.is_finite() && bpm > 0.0,
        "bpm must be finite and > 0, got {bpm}"
    );
}

fn check_sample_rate(sample_rate: u32) {
    assert!(sample_rate > 0, "sample_rate must be > 0");
}

fn check_segments(segments: &[TempoSegment]) {
    assert!(!segments.is_empty(), "need at least one tempo segment");
    for s in segments {
        check_bpm(s.bpm);
        assert!(
            s.seconds.is_finite() && s.seconds > 0.0,
            "segment seconds must be finite and > 0, got {}",
            s.seconds
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    /// 64-bit FNV-1a: a tiny, stable hash so the test does not depend on
    /// std's unspecified `DefaultHasher` or an extra crate.
    fn fnv1a(bytes: &[u8]) -> u64 {
        bytes.iter().fold(0xcbf2_9ce4_8422_2325, |h, &b| {
            (h ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01b3)
        })
    }

    fn read_wav(path: &std::path::Path) -> (hound::WavSpec, Vec<i16>) {
        let mut r = hound::WavReader::open(path).unwrap();
        let spec = r.spec();
        let samples = r.samples::<i16>().map(Result::unwrap).collect();
        (spec, samples)
    }

    /// Sample indices where a click starts: first non-silent sample after at
    /// least 1 ms of digital silence (or at the very start).
    fn detect_onsets(samples: &[i16], sample_rate: u32) -> Vec<usize> {
        let quiet_needed = sample_rate as usize / 1000;
        let mut quiet = quiet_needed;
        let mut onsets = Vec::new();
        for (i, &s) in samples.iter().enumerate() {
            if s == 0 {
                quiet += 1;
            } else {
                if quiet >= quiet_needed {
                    onsets.push(i);
                }
                quiet = 0;
            }
        }
        onsets
    }

    fn assert_onsets_match(samples: &[i16], sample_rate: u32, beats: &[f64]) {
        let onsets = detect_onsets(samples, sample_rate);
        assert_eq!(onsets.len(), beats.len(), "one click per beat");
        for (&onset, &t) in onsets.iter().zip(beats) {
            let expected = (t * f64::from(sample_rate)).round() as usize;
            // The sine starts at phase 0, so sample `expected` itself is 0 and
            // the first audible sample is the next one.
            assert!(
                onset >= expected && onset <= expected + 1,
                "click for beat {t}s at sample {onset}, expected {expected}"
            );
        }
    }

    #[test]
    fn beat_times_ground_truth() {
        assert_eq!(beat_times(120.0, 2.0), vec![0.0, 0.5, 1.0, 1.5]);
        assert_eq!(beat_times(60.0, 3.5), vec![0.0, 1.0, 2.0, 3.0]);
        assert_eq!(beat_times(120.0, 0.0), Vec::<f64>::new());
        let b = beat_times(128.0, 30.0);
        assert_eq!(b[0], 0.0);
        assert_eq!(b.len(), 64); // 128 BPM * 0.5 min
        for w in b.windows(2) {
            assert!((w[1] - w[0] - 60.0 / 128.0).abs() < 1e-12);
        }
    }

    #[test]
    fn tempo_change_ground_truth() {
        let b = tempo_change_beat_times(&TEMPO_CHANGE_100_140);
        let (first, second): (Vec<f64>, Vec<f64>) = b.iter().partition(|&&t| t < 8.0);
        // 100 BPM over 8 s: beats at 0, 0.6, ..., 7.8.
        assert_eq!(first.len(), 14);
        assert_eq!(first[0], 0.0);
        assert!((first[13] - 7.8).abs() < 1e-12);
        // 140 BPM over 8 s starting at 8 s: 8 * 140 / 60 = 18.67 -> 19 beats.
        assert_eq!(second.len(), 19);
        assert_eq!(second[0], 8.0);
        for w in first.windows(2) {
            assert!((w[1] - w[0] - 0.6).abs() < 1e-12);
        }
        for w in second.windows(2) {
            assert!((w[1] - w[0] - 60.0 / 140.0).abs() < 1e-12);
        }
        assert!(b.iter().all(|&t| t < 16.0));
    }

    #[test]
    fn click_track_file_matches_ground_truth() {
        let path = click_track(120.0, 4.0, DEFAULT_SAMPLE_RATE);
        assert!(path.starts_with(crate::fixtures_dir()));
        assert_eq!(path.extension().unwrap(), "wav");
        let (spec, samples) = read_wav(&path);
        assert_eq!(spec.channels, 1);
        assert_eq!(spec.sample_rate, DEFAULT_SAMPLE_RATE);
        assert_eq!(spec.bits_per_sample, 16);
        assert_eq!(spec.sample_format, hound::SampleFormat::Int);
        assert_eq!(samples.len(), 4 * DEFAULT_SAMPLE_RATE as usize);
        assert_onsets_match(&samples, DEFAULT_SAMPLE_RATE, &beat_times(120.0, 4.0));
        // First beat at t = 0: the click occupies the first 10 ms.
        assert!(samples[..441].iter().any(|&s| s.unsigned_abs() > 10_000));
    }

    #[test]
    fn click_is_short_and_sharp() {
        let path = click_track(60.0, 2.0, 48_000);
        let (spec, samples) = read_wav(&path);
        assert_eq!(spec.sample_rate, 48_000);
        // Peak within the first millisecond (sharp transient).
        let peak_idx = (0..480).max_by_key(|&i| samples[i].unsigned_abs()).unwrap();
        assert!(peak_idx < 48, "peak at sample {peak_idx}");
        // Silent from 10 ms until the next beat at 1 s.
        assert!(samples[480..48_000].iter().all(|&s| s == 0));
        assert_onsets_match(&samples, 48_000, &[0.0, 1.0]);
    }

    #[test]
    fn tempo_change_file_matches_ground_truth() {
        let path = tempo_change_track(&TEMPO_CHANGE_100_140, DEFAULT_SAMPLE_RATE);
        assert!(path.starts_with(crate::fixtures_dir()));
        let (spec, samples) = read_wav(&path);
        assert_eq!(spec.channels, 1);
        assert_eq!(spec.bits_per_sample, 16);
        assert_eq!(samples.len(), 16 * DEFAULT_SAMPLE_RATE as usize);
        assert_onsets_match(
            &samples,
            DEFAULT_SAMPLE_RATE,
            &tempo_change_beat_times(&TEMPO_CHANGE_100_140),
        );
    }

    #[test]
    fn rendering_is_deterministic() {
        let a = fnv1a(&render_click_track_wav(100.0, 3.0, DEFAULT_SAMPLE_RATE));
        let b = fnv1a(&render_click_track_wav(100.0, 3.0, DEFAULT_SAMPLE_RATE));
        assert_eq!(a, b);
        let c = fnv1a(&render_click_track_wav(101.0, 3.0, DEFAULT_SAMPLE_RATE));
        assert_ne!(a, c, "different parameters give different files");

        let t1 = fnv1a(&render_tempo_change_wav(
            &TEMPO_CHANGE_100_140,
            DEFAULT_SAMPLE_RATE,
        ));
        let t2 = fnv1a(&render_tempo_change_wav(
            &TEMPO_CHANGE_100_140,
            DEFAULT_SAMPLE_RATE,
        ));
        assert_eq!(t1, t2);
    }

    #[test]
    fn cached_files_are_byte_identical_to_fresh_renders() {
        let p1 = click_track(90.0, 2.5, 22_050);
        let p2 = click_track(90.0, 2.5, 22_050);
        assert_eq!(p1, p2, "same parameters reuse the same file");
        assert_eq!(
            fnv1a(&std::fs::read(&p1).unwrap()),
            fnv1a(&render_click_track_wav(90.0, 2.5, 22_050))
        );

        let t = tempo_change_track(&TEMPO_CHANGE_100_140, DEFAULT_SAMPLE_RATE);
        assert_eq!(
            fnv1a(&std::fs::read(&t).unwrap()),
            fnv1a(&render_tempo_change_wav(
                &TEMPO_CHANGE_100_140,
                DEFAULT_SAMPLE_RATE
            ))
        );
    }

    #[test]
    fn distinct_parameters_get_distinct_files() {
        let a = click_track(120.0, 1.0, DEFAULT_SAMPLE_RATE);
        let b = click_track(120.0, 1.5, DEFAULT_SAMPLE_RATE);
        let c = click_track(120.0, 1.0, 48_000);
        assert_ne!(a, b);
        assert_ne!(a, c);
    }

    #[test]
    fn rendering_is_fast() {
        let start = Instant::now();
        render_tempo_change_wav(&TEMPO_CHANGE_100_140, DEFAULT_SAMPLE_RATE);
        render_click_track_wav(120.0, 16.0, DEFAULT_SAMPLE_RATE);
        assert!(start.elapsed() < Duration::from_secs(1));
    }

    #[test]
    #[should_panic(expected = "bpm must be finite and > 0")]
    fn rejects_zero_bpm() {
        beat_times(0.0, 1.0);
    }

    #[test]
    #[should_panic(expected = "need at least one tempo segment")]
    fn rejects_empty_segments() {
        tempo_change_beat_times(&[]);
    }
}
