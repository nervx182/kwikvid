//! `kv_audio::decode_range` and `render_clip_audio` on MP4 clips with an AAC
//! tone track, generated with FFmpeg (`kv_testkit::tagged_clip`).

use std::f32::consts::TAU;

use kv_audio::{decode_range, render_clip_audio, ClipAudioOptions, ClipAudioSource, Error};
use kv_core::{Edl, MediaId, Micros, Segment, Speed, Transition};
use kv_testkit::{tagged_clip, ClipSpec};

/// Amplitude of the fixtures' tone.
const TONE: f32 = 0.05;

fn clip(audio_hz: Option<u32>) -> std::path::PathBuf {
    tagged_clip(&ClipSpec {
        clip: 1,
        audio: audio_hz.is_some(),
        audio_hz: audio_hz.unwrap_or(440),
        ..ClipSpec::default()
    })
}

/// Amplitude of the `freq` Hz component of `x` (Hann-windowed single-bin
/// DFT, normalised so a sine of amplitude `a` gives `a`).
fn tone_level(x: &[f32], freq: f32, rate: u32) -> f32 {
    let n = x.len() as f64;
    let (mut re, mut im, mut wsum) = (0.0f64, 0.0f64, 0.0f64);
    for (i, &v) in x.iter().enumerate() {
        let w = 0.5 - 0.5 * (std::f64::consts::TAU * i as f64 / n).cos();
        let ph = f64::from(TAU * freq) * i as f64 / f64::from(rate);
        re += w * f64::from(v) * ph.cos();
        im += w * f64::from(v) * ph.sin();
        wsum += w;
    }
    (2.0 * (re * re + im * im).sqrt() / wsum) as f32
}

fn channel(samples: &[f32], channels: usize, c: usize) -> Vec<f32> {
    samples.chunks_exact(channels).map(|f| f[c]).collect()
}

#[test]
fn decodes_a_range_of_an_mp4_aac_track_resampled() {
    let path = clip(Some(1_000));
    let pcm = decode_range(&path, Micros(500_000), Micros(1_250_000), 44_100, 2).unwrap();
    assert_eq!((pcm.sample_rate, pcm.channels), (44_100, 2));
    assert_eq!(pcm.frames(), 33_075);
    // Mono source on both channels, at the fixture's level.
    for c in 0..2 {
        let x = channel(&pcm.samples, 2, c);
        let level = tone_level(&x, 1_000.0, 44_100);
        assert!((level - TONE).abs() < 0.1 * TONE, "channel {c}: {level}");
        assert!(tone_level(&x, 440.0, 44_100) < 0.05 * TONE);
        // The tone fills the whole range: no gap at either end.
        for part in [&x[..2_205], &x[x.len() - 2_205..]] {
            let l = tone_level(part, 1_000.0, 44_100);
            assert!(l > 0.8 * TONE, "edge level {l}");
        }
    }
    // Same rate: no resampling, still exact.
    let mono = decode_range(&path, Micros(1_000_000), Micros(1_100_000), 48_000, 1).unwrap();
    assert_eq!(mono.frames(), 4_800);
    assert!((tone_level(&mono.samples, 1_000.0, 48_000) - TONE).abs() < 0.1 * TONE);
}

#[test]
fn past_the_end_is_silence() {
    let path = clip(Some(1_000));
    // The clip lasts 2 s: [1.5, 3) is half tone, half silence.
    let pcm = decode_range(&path, Micros(1_500_000), Micros(3_000_000), 48_000, 1).unwrap();
    assert_eq!(pcm.frames(), 72_000);
    assert!(tone_level(&pcm.samples[..19_200], 1_000.0, 48_000) > 0.8 * TONE);
    assert!(pcm.samples[33_600..].iter().all(|&v| v.abs() < 1e-3));
    let after = decode_range(&path, Micros(5_000_000), Micros(6_000_000), 48_000, 2).unwrap();
    assert_eq!(after.frames(), 48_000);
    assert!(after.samples.iter().all(|&v| v == 0.0));
}

#[test]
fn file_without_audio_is_silence_of_the_right_length() {
    let path = clip(None);
    let pcm = decode_range(&path, Micros(250_000), Micros(1_000_000), 48_000, 2).unwrap();
    assert_eq!(pcm.frames(), 36_000);
    assert_eq!(pcm.samples.len(), 72_000);
    assert!(pcm.samples.iter().all(|&v| v == 0.0));
}

#[test]
fn missing_file_is_an_error() {
    let err = decode_range("/no/such/clip.mp4", Micros(0), Micros(1), 48_000, 2).unwrap_err();
    assert!(matches!(err, Error::Io(_)), "{err}");
    let edl = Edl {
        segments: vec![Segment {
            media_id: MediaId(7),
            src_in: Micros(0),
            src_out: Micros(1_000_000),
            timeline_start: Micros(0),
            transition: Transition::Cut,
            speed: Speed::NORMAL,
        }],
    };
    let sources = [ClipAudioSource {
        media_id: MediaId(7),
        path: "/no/such/clip.mp4".into(),
    }];
    let opts = ClipAudioOptions {
        sample_rate: 48_000,
        channels: 2,
        length: Micros(1_000_000),
    };
    match render_clip_audio(&edl, &sources, &opts) {
        Err(Error::ClipSource { media_id, .. }) => assert_eq!(media_id, MediaId(7)),
        other => panic!("{other:?}"),
    }
}

#[test]
fn renders_tone_only_inside_its_segment() {
    // Media 1 (1 kHz) for 1 s, then media 2 (no clip audio) for 1 s.
    let mut edl = Edl {
        segments: [1, 2]
            .map(|id| Segment {
                media_id: MediaId(id),
                src_in: Micros(200_000),
                src_out: Micros(1_200_000),
                timeline_start: Micros(0),
                transition: Transition::Cut,
                speed: Speed::NORMAL,
            })
            .to_vec(),
    };
    edl.repack();
    let sources = [ClipAudioSource {
        media_id: MediaId(1),
        path: clip(Some(1_000)),
    }];
    let opts = ClipAudioOptions {
        sample_rate: 48_000,
        channels: 1,
        length: Micros(2_000_000),
    };
    let pcm = render_clip_audio(&edl, &sources, &opts).unwrap();
    assert_eq!(pcm.frames(), 96_000);
    // 10 ms fade, then full level until 10 ms before the cut.
    assert!(tone_level(&pcm.samples[480..47_520], 1_000.0, 48_000) > 0.9 * TONE);
    assert!(pcm.samples[48_000..].iter().all(|&v| v == 0.0));
}
