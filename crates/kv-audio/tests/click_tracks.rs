//! Accuracy of `kv_audio::analyze` on kv-testkit click tracks with known beats.

use std::path::PathBuf;
use std::time::Instant;

use kv_audio::{analyze, Error, MusicAnalysis, BEATS_PER_BAR};
use kv_core::Micros;
use kv_testkit::{
    beat_times, click_track, fixtures_dir, tempo_change_beat_times, tempo_change_track,
    DEFAULT_SAMPLE_RATE, TEMPO_CHANGE_100_140,
};

fn secs(m: &[Micros]) -> Vec<f64> {
    m.iter().map(|t| t.0 as f64 / 1e6).collect()
}

/// Number of one-to-one matches between `truth` and `est` within `window`
/// seconds (greedy on sorted lists; exact for well-separated beats).
fn matches(truth: &[f64], est: &[f64], window: f64) -> usize {
    let mut used = vec![false; est.len()];
    let mut n = 0;
    for &t in truth {
        let best = est
            .iter()
            .enumerate()
            .filter(|&(i, &e)| !used[i] && (e - t).abs() <= window)
            .min_by(|a, b| (a.1 - t).abs().total_cmp(&(b.1 - t).abs()));
        if let Some((i, _)) = best {
            used[i] = true;
            n += 1;
        }
    }
    n
}

fn f_measure(truth: &[f64], est: &[f64], window: f64) -> f64 {
    if truth.is_empty() || est.is_empty() {
        return 0.0;
    }
    let m = matches(truth, est, window) as f64;
    let p = m / est.len() as f64;
    let r = m / truth.len() as f64;
    if m == 0.0 {
        0.0
    } else {
        2.0 * p * r / (p + r)
    }
}

fn check_basic_invariants(a: &MusicAnalysis) {
    assert!(a.beats.windows(2).all(|w| w[0] < w[1]), "beats increasing");
    assert!(
        a.onsets.windows(2).all(|w| w[0] < w[1]),
        "onsets increasing"
    );
    assert!(a.beats.iter().all(|b| b.0 >= 0 && *b <= a.duration));
    assert!(a.downbeats.iter().all(|d| a.beats.contains(d)));
    let expected_bars = a.beats.len().div_ceil(BEATS_PER_BAR);
    assert!(a.downbeats.len().abs_diff(expected_bars) <= 1);
    let blocks = (a.duration.0 as f64 / 100_000.0).ceil() as usize;
    assert!(a.energy.len().abs_diff(blocks) <= 1, "energy per 100 ms");
}

#[test]
fn click_tracks_tempo_and_beats() {
    for bpm in [60.0, 100.0, 128.0, 174.0] {
        let path = click_track(bpm, 20.0, DEFAULT_SAMPLE_RATE);
        let start = Instant::now();
        let a = analyze(&path).unwrap();
        let elapsed = start.elapsed();
        eprintln!(
            "{bpm} BPM: tempo {:.2}, {} beats, analysed in {elapsed:?}",
            a.tempo_bpm,
            a.beats.len()
        );
        check_basic_invariants(&a);
        assert_eq!(a.duration, Micros(20_000_000));
        assert!(
            (a.tempo_bpm - bpm).abs() <= 1.0,
            "{bpm} BPM click track reported {} BPM",
            a.tempo_bpm
        );

        let truth = beat_times(bpm, 20.0);
        let est = secs(&a.beats);
        let m = matches(&truth, &est, 0.050);
        assert!(
            m as f64 >= 0.95 * truth.len() as f64,
            "{bpm} BPM: {m}/{} beats within 50 ms; got {est:?}",
            truth.len()
        );
        // No spurious extra beats either.
        assert!(
            est.len() <= truth.len() + 1,
            "{bpm} BPM: {} beats",
            est.len()
        );

        // Every click is an onset.
        let onsets = secs(&a.onsets);
        assert!(matches(&truth, &onsets, 0.050) as f64 >= 0.95 * truth.len() as f64);

        // Downbeats: every 4th beat from the first click.
        let first = a.downbeats.first().expect("downbeats").0 as f64 / 1e6;
        assert!(first.abs() < 0.05, "first downbeat at {first}");
    }
}

#[test]
fn tempo_change_beats_follow_local_tempo() {
    let path = tempo_change_track(&TEMPO_CHANGE_100_140, DEFAULT_SAMPLE_RATE);
    let a = analyze(&path).unwrap();
    check_basic_invariants(&a);
    let truth = tempo_change_beat_times(&TEMPO_CHANGE_100_140);
    let est = secs(&a.beats);
    let split = TEMPO_CHANGE_100_140[0].seconds;
    for (name, keep) in [
        (
            "first half",
            Box::new(|t: f64| t < split) as Box<dyn Fn(f64) -> bool>,
        ),
        ("second half", Box::new(|t: f64| t >= split)),
    ] {
        // Assign estimated beats to halves with a small margin so a beat on
        // the boundary is not double-counted.
        let t: Vec<f64> = truth.iter().copied().filter(|&x| keep(x)).collect();
        let e: Vec<f64> = est.iter().copied().filter(|&x| keep(x + 0.035)).collect();
        let f = f_measure(&t, &e, 0.070);
        eprintln!(
            "tempo change {name}: F = {f:.3} ({} est, {} truth)",
            e.len(),
            t.len()
        );
        assert!(f >= 0.85, "{name}: F-measure {f:.3}; est {e:?}");
    }
    assert!(
        (a.tempo_bpm - 140.0).abs() <= 1.0 || (a.tempo_bpm - 100.0).abs() <= 1.0,
        "dominant tempo {}",
        a.tempo_bpm
    );
}

/// Write a WAV with hound and return its path in the fixture cache.
fn write_wav(
    name: &str,
    channels: u16,
    sample_rate: u32,
    frames: impl Fn(usize) -> f32,
) -> PathBuf {
    let dir = fixtures_dir();
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("kv-audio-{name}-{}.wav", std::process::id()));
    let spec = hound::WavSpec {
        channels,
        sample_rate,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut w = hound::WavWriter::create(&path, spec).unwrap();
    for i in 0..(sample_rate as usize * 10) {
        let s = frames(i);
        for c in 0..channels {
            // Put the signal in one channel only for stereo so mixdown is
            // exercised.
            let v = if c == 0 { s } else { 0.0 };
            w.write_sample((v * f32::from(i16::MAX)) as i16).unwrap();
        }
    }
    w.finalize().unwrap();
    path
}

#[test]
fn stereo_48k_is_mixed_down_and_resampled() {
    // 120 BPM clicks: a 5 ms 1 kHz burst every 0.5 s, left channel only.
    let sr = 48_000;
    let path = write_wav("stereo48k", 2, sr, |i| {
        let t = (i % (sr as usize / 2)) as f32 / sr as f32;
        if t < 0.005 {
            0.9 * (std::f32::consts::TAU * 1000.0 * t).sin()
        } else {
            0.0
        }
    });
    let a = analyze(&path).unwrap();
    std::fs::remove_file(&path).unwrap();
    assert_eq!(a.duration, Micros(10_000_000));
    assert!((a.tempo_bpm - 120.0).abs() <= 1.0, "tempo {}", a.tempo_bpm);
    let truth = beat_times(120.0, 10.0);
    let m = matches(&truth, &secs(&a.beats), 0.050);
    assert!(m as f64 >= 0.95 * truth.len() as f64, "{m}/{}", truth.len());
}

#[test]
fn missing_file_is_an_io_error() {
    let err = analyze(fixtures_dir().join("does-not-exist.wav")).unwrap_err();
    assert!(matches!(err, Error::Io(_)), "{err}");
}

#[test]
fn garbage_file_is_a_decode_error() {
    let dir = fixtures_dir();
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("kv-audio-garbage-{}.mp3", std::process::id()));
    std::fs::write(&path, vec![0x5au8; 4096]).unwrap();
    let err = analyze(&path).unwrap_err();
    std::fs::remove_file(&path).unwrap();
    assert!(matches!(err, Error::Decode(_)), "{err}");
}

/// Logs the analysis time of a 20 s file (target: under 200 ms in release on
/// CI). Not asserted, to keep the gate free of timing flakiness.
#[test]
fn analysis_time_is_logged() {
    let path = click_track(128.0, 20.0, DEFAULT_SAMPLE_RATE);
    analyze(&path).unwrap(); // warm up (page cache, FFT plans)
    let start = Instant::now();
    analyze(&path).unwrap();
    let profile = if cfg!(debug_assertions) {
        "debug"
    } else {
        "release"
    };
    eprintln!(
        "analysis of a 20 s file took {:?} ({profile})",
        start.elapsed()
    );
}

// TODO(#7): real-music accuracy against hand-annotated beats belongs in the
// regression suite (`#[ignore = "regression"]`) once legally usable sample
// music with annotations is available. MP3/AAC/FLAC decoding is enabled but
// only WAV is exercised here, because no permissively licensed encoder is in
// the dependency tree to generate those fixtures.
