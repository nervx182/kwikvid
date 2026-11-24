//! `kv_audio::render_music_bed` on WAV files generated in the test.

use std::f32::consts::TAU;
use std::path::PathBuf;

use kv_audio::{render_music_bed, Error, MusicBedOptions, PcmAudio};
use kv_core::Micros;
use kv_testkit::{click_track, fixtures_dir, DEFAULT_SAMPLE_RATE};

/// Write a 16-bit WAV of `seconds` with `channels` channels; `sample(frame,
/// channel)` gives each sample. Returns its path (unique per process).
fn write_wav(
    name: &str,
    channels: u16,
    sample_rate: u32,
    seconds: f64,
    sample: impl Fn(usize, u16) -> f32,
) -> PathBuf {
    let dir = fixtures_dir();
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("kv-audio-bed-{name}-{}.wav", std::process::id()));
    let spec = hound::WavSpec {
        channels,
        sample_rate,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut w = hound::WavWriter::create(&path, spec).unwrap();
    for i in 0..(f64::from(sample_rate) * seconds).round() as usize {
        for c in 0..channels {
            w.write_sample((sample(i, c) * f32::from(i16::MAX)) as i16)
                .unwrap();
        }
    }
    w.finalize().unwrap();
    path
}

/// Left: 440 Hz at amplitude 0.8. Right: 1 kHz at amplitude 0.4.
fn two_tone(i: usize, c: u16) -> f32 {
    let t = i as f32 / 44_100.0;
    if c == 0 {
        0.8 * (TAU * 440.0 * t).sin()
    } else {
        0.4 * (TAU * 1_000.0 * t).sin()
    }
}

fn channel(pcm: &PcmAudio, c: usize) -> Vec<f32> {
    pcm.samples
        .chunks_exact(usize::from(pcm.channels))
        .map(|f| f[c])
        .collect()
}

fn peak(x: &[f32]) -> f32 {
    x.iter().fold(0.0, |m, &v| m.max(v.abs()))
}

/// Correlation of `x` with a sine of `freq` Hz at `rate` Hz (magnitude of the
/// single-bin DFT, normalised to the amplitude of a matching sine).
fn tone_level(x: &[f32], freq: f32, rate: u32) -> f32 {
    let (mut re, mut im) = (0.0f64, 0.0f64);
    for (i, &v) in x.iter().enumerate() {
        let ph = f64::from(TAU * freq) * i as f64 / f64::from(rate);
        re += f64::from(v) * ph.cos();
        im += f64::from(v) * ph.sin();
    }
    (2.0 * (re * re + im * im).sqrt() / x.len() as f64) as f32
}

#[test]
fn stereo_44k1_to_48k_keeps_channels_volume_and_fades_out() {
    let path = write_wav("two-tone", 2, 44_100, 3.0, two_tone);
    let mut opts = MusicBedOptions::new(48_000, 2, Micros(2_000_000));
    opts.start = Micros(500_000);
    opts.volume_percent = 50;
    let pcm = render_music_bed(&path, &opts).unwrap();
    std::fs::remove_file(&path).unwrap();

    assert_eq!(pcm.sample_rate, 48_000);
    assert_eq!(pcm.channels, 2);
    assert_eq!(pcm.frames(), 96_000);
    assert_eq!(pcm.samples.len(), 192_000);

    // Before the 1 s fade-out (first second), check order and level.
    let (l, r) = (channel(&pcm, 0), channel(&pcm, 1));
    let head = 0..48_000;
    let (l_head, r_head) = (&l[head.clone()], &r[head]);
    assert!((tone_level(l_head, 440.0, 48_000) - 0.4).abs() < 0.01);
    assert!(
        tone_level(l_head, 1_000.0, 48_000) < 0.01,
        "right leaked left"
    );
    assert!((tone_level(r_head, 1_000.0, 48_000) - 0.2).abs() < 0.01);
    assert!(
        tone_level(r_head, 440.0, 48_000) < 0.01,
        "left leaked right"
    );
    // Peak follows volume (50% of 0.8 and 0.4).
    assert!(
        (peak(l_head) - 0.4).abs() < 0.01,
        "left peak {}",
        peak(l_head)
    );
    assert!(
        (peak(r_head) - 0.2).abs() < 0.01,
        "right peak {}",
        peak(r_head)
    );

    // Fade-out: last frame silent, last 10 ms quiet, all in range.
    assert!(l[95_999].abs() < 1e-6 && r[95_999].abs() < 1e-6);
    assert!(peak(&l[95_520..]) < 0.01);
    assert!(pcm.samples.iter().all(|s| (-1.0..=1.0).contains(s)));
}

#[test]
fn volume_100_without_fades_matches_source_level() {
    let path = write_wav("two-tone-full", 2, 44_100, 1.0, two_tone);
    let mut opts = MusicBedOptions::new(48_000, 2, Micros(1_000_000));
    opts.fade_out = Micros(0);
    let pcm = render_music_bed(&path, &opts).unwrap();
    std::fs::remove_file(&path).unwrap();
    assert_eq!(pcm.frames(), 48_000);
    let l = channel(&pcm, 0);
    assert!((peak(&l[1_000..47_000]) - 0.8).abs() < 0.01);
}

#[test]
fn short_song_is_padded_with_silence() {
    // 1 s of a constant-ish tone, bed of 2.5 s starting 0.25 s in.
    let path = write_wav("short", 2, 48_000, 1.0, |i, _| {
        0.5 * (TAU * 300.0 * i as f32 / 48_000.0).sin()
    });
    let mut opts = MusicBedOptions::new(48_000, 2, Micros(2_500_000));
    opts.start = Micros(250_000);
    opts.fade_out = Micros(0);
    let pcm = render_music_bed(&path, &opts).unwrap();
    std::fs::remove_file(&path).unwrap();
    assert_eq!(pcm.frames(), 120_000);
    let l = channel(&pcm, 0);
    // 0.75 s of song left after the offset.
    assert!(peak(&l[..36_000]) > 0.45);
    assert!(l[36_000..].iter().all(|&v| v == 0.0), "silence after song");

    // Same with resampling: the tail is silent too.
    opts.sample_rate = 44_100;
    let path = write_wav("short-rs", 2, 48_000, 1.0, |i, _| {
        0.5 * (TAU * 300.0 * i as f32 / 48_000.0).sin()
    });
    let pcm = render_music_bed(&path, &opts).unwrap();
    std::fs::remove_file(&path).unwrap();
    assert_eq!(pcm.frames(), 110_250);
    let l = channel(&pcm, 0);
    assert!(peak(&l[..33_000]) > 0.45);
    assert!(peak(&l[34_000..]) < 1e-3, "silence after song");

    // Starting past the end gives all silence.
    opts.start = Micros(5_000_000);
    let path = write_wav("short-past", 1, 48_000, 0.5, |_, _| 0.5);
    let pcm = render_music_bed(&path, &opts).unwrap();
    std::fs::remove_file(&path).unwrap();
    assert_eq!(pcm.frames(), 110_250);
    assert!(pcm.samples.iter().all(|&v| v == 0.0));
}

#[test]
fn mono_click_track_becomes_two_identical_channels() {
    let path = click_track(120.0, 4.0, DEFAULT_SAMPLE_RATE);
    let opts = MusicBedOptions::new(48_000, 2, Micros(3_000_000));
    let pcm = render_music_bed(&path, &opts).unwrap();
    assert_eq!(pcm.channels, 2);
    assert_eq!(pcm.frames(), 144_000);
    let (l, r) = (channel(&pcm, 0), channel(&pcm, 1));
    assert_eq!(l, r);
    assert!(peak(&l) > 0.1, "clicks survive, peak {}", peak(&l));
}

#[test]
fn mono_output_from_stereo() {
    let path = write_wav("to-mono", 2, 44_100, 1.0, two_tone);
    let mut opts = MusicBedOptions::new(44_100, 1, Micros(1_000_000));
    opts.fade_out = Micros(0);
    let pcm = render_music_bed(&path, &opts).unwrap();
    std::fs::remove_file(&path).unwrap();
    assert_eq!(pcm.channels, 1);
    assert_eq!(pcm.frames(), 44_100);
    // Average of both channels: each tone at half its level.
    assert!((tone_level(&pcm.samples, 440.0, 44_100) - 0.4).abs() < 0.01);
    assert!((tone_level(&pcm.samples, 1_000.0, 44_100) - 0.2).abs() < 0.01);
}

#[test]
fn invalid_options_and_missing_file() {
    let path = click_track(120.0, 4.0, DEFAULT_SAMPLE_RATE);
    let opts = MusicBedOptions::new(48_000, 6, Micros(1_000_000));
    assert!(matches!(
        render_music_bed(&path, &opts),
        Err(Error::InvalidOptions(_))
    ));
    let opts = MusicBedOptions::new(48_000, 2, Micros(1_000_000));
    let err = render_music_bed(fixtures_dir().join("nope.wav"), &opts).unwrap_err();
    assert!(matches!(err, Error::Io(_)), "{err}");
}
