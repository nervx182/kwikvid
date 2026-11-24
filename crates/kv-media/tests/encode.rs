//! Encoder tests: write 2 s of tagged 320x240 frames plus a stereo sine,
//! then probe the file and decode it (with `ffmpeg-next` through
//! `kv_testkit::read_clip_tags`, not kv-media's own decoder) to check every
//! frame.
#![cfg(feature = "ffmpeg")]

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use ffmpeg_next as ff;
use kv_core::Micros;
use kv_media::{
    Encoder, ExportSettings, FfmpegBackend, MediaBackend, MediaError, Rational, VideoFrame,
    VideoWriter,
};
use kv_testkit::{read_clip_tags, render_tagged_frame, FrameTag};

const W: u32 = 320;
const H: u32 = 240;
const FPS: i32 = 30;
const FRAMES: usize = 60;
const RATE: u32 = 48_000;
const CLIP: u8 = 5;

fn out_dir() -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("kv-media-encode");
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn settings() -> ExportSettings {
    ExportSettings {
        width: W,
        height: H,
        fps: Rational::new(FPS, 1),
        video_bitrate: 2_000_000,
        audio_sample_rate: RATE,
        audio_channels: 2,
        ..ExportSettings::default()
    }
}

fn backend() -> FfmpegBackend {
    FfmpegBackend::new().expect("FFmpeg initialises")
}

fn frame_pts(i: usize) -> Micros {
    Micros(i as i64 * 1_000_000 / i64::from(FPS))
}

fn tagged(i: usize) -> VideoFrame {
    let tag = FrameTag {
        clip: CLIP,
        frame: i as u16,
    };
    VideoFrame::new_rgba(frame_pts(i), W, H, render_tagged_frame(tag, W, H)).unwrap()
}

/// Interleaved stereo 440 Hz (left) / 660 Hz (right) sine, samples `from..to`.
fn sine(from: usize, to: usize) -> Vec<f32> {
    (from..to)
        .flat_map(|n| {
            let t = n as f32 / RATE as f32;
            [
                0.3 * (t * 440.0 * std::f32::consts::TAU).sin(),
                0.3 * (t * 660.0 * std::f32::consts::TAU).sin(),
            ]
        })
        .collect()
}

/// A tracing subscriber that records `warn!` and `error!` messages.
#[derive(Clone, Default)]
struct WarnLog(Arc<Mutex<Vec<String>>>);

impl tracing::Subscriber for WarnLog {
    fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
        true
    }
    fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }
    fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
    fn event(&self, event: &tracing::Event<'_>) {
        struct Msg(String);
        impl tracing::field::Visit for Msg {
            fn record_debug(&mut self, f: &tracing::field::Field, v: &dyn std::fmt::Debug) {
                let _ = write!(self.0, "{}={v:?} ", f.name());
            }
        }
        if *event.metadata().level() <= tracing::Level::WARN {
            let mut m = Msg(String::new());
            event.record(&mut m);
            self.0.lock().unwrap().push(m.0);
        }
    }
    fn enter(&self, _: &tracing::span::Id) {}
    fn exit(&self, _: &tracing::span::Id) {}
}

/// `create` with warnings captured.
fn create_logged(path: &Path, s: &ExportSettings) -> (Box<dyn VideoWriter>, Vec<String>) {
    let log = WarnLog::default();
    let writer = tracing::subscriber::with_default(log.clone(), || backend().create(path, s))
        .expect("create writer");
    let warnings = log.0.lock().unwrap().clone();
    (writer, warnings)
}

/// Write the standard 2 s clip; audio arrives in uneven chunks interleaved
/// with the video. Returns the encoder name and warnings.
fn write_standard(path: &Path, with_audio: bool) -> (String, Vec<String>) {
    let (mut w, warnings) = create_logged(path, &settings());
    let name = w.encoder_name().to_owned();
    let total = 2 * RATE as usize;
    let mut sent = 0;
    for i in 0..FRAMES {
        w.write_video(&tagged(i)).unwrap();
        if with_audio {
            // 1, 1601, 3201, ... samples: never a multiple of 1024.
            let want = ((i + 1) * total / FRAMES).min(sent + 1 + (i % 3) * 1600);
            let want = if i == FRAMES - 1 { total } else { want };
            w.write_audio(&sine(sent, want)).unwrap();
            sent = want;
        }
    }
    w.finish().unwrap();
    (name, warnings)
}

fn assert_near_two_seconds(d: Micros, what: &str) {
    let frame = 1_000_000 / i64::from(FPS);
    assert!(
        (d.0 - 2_000_000).abs() <= frame,
        "{what} {d:?} is not 2 s +- one frame"
    );
}

#[test]
fn encodes_h264_aac_mp4_with_every_tagged_frame_in_order() {
    let path = out_dir().join("tagged_h264_aac.mp4");
    let (encoder, _) = write_standard(&path, true);
    println!("encoded with {encoder}");

    let info = backend().probe(&path).unwrap();
    let v = info.video.expect("video stream");
    assert_eq!(v.codec, "h264");
    assert_eq!((v.width, v.height), (W, H));
    assert_eq!(v.fps, Rational::new(FPS, 1));
    let a = info.audio.expect("audio stream");
    assert_eq!(a.codec, "aac");
    assert_eq!(a.sample_rate, RATE);
    assert_eq!(a.channels, 2);
    assert_near_two_seconds(info.duration, "container duration");

    let tags = read_clip_tags(&path);
    let expected: Vec<_> = (0..FRAMES)
        .map(|i| {
            Some(FrameTag {
                clip: CLIP,
                frame: i as u16,
            })
        })
        .collect();
    assert_eq!(tags, expected);

    // Streams start at 0, have the right length, carry BT.709 limited range.
    let ictx = ff::format::input(&path).unwrap();
    for s in ictx.streams() {
        let tb = s.time_base();
        let to_us =
            |t: i64| t * 1_000_000 * i64::from(tb.numerator()) / i64::from(tb.denominator());
        assert_eq!(to_us(s.start_time()), 0, "stream {} start", s.index());
        assert_near_two_seconds(Micros(to_us(s.duration())), "stream duration");
        if s.parameters().medium() == ff::media::Type::Video {
            let dec = ff::codec::Context::from_parameters(s.parameters())
                .unwrap()
                .decoder()
                .video()
                .unwrap();
            assert_eq!(dec.color_space(), ff::color::Space::BT709);
            assert_eq!(dec.color_range(), ff::color::Range::MPEG);
            assert_eq!(dec.color_primaries(), ff::color::Primaries::BT709);
            assert_eq!(
                dec.color_transfer_characteristic(),
                ff::color::TransferCharacteristic::BT709
            );
        }
    }

    // +faststart: moov comes before mdat at the top level.
    let boxes = top_level_boxes(&std::fs::read(&path).unwrap());
    let pos = |t: &str| boxes.iter().position(|b| b == t);
    assert!(
        pos("moov").unwrap() < pos("mdat").unwrap(),
        "boxes {boxes:?}"
    );
}

fn top_level_boxes(bytes: &[u8]) -> Vec<String> {
    let mut out = Vec::new();
    let mut at = 0usize;
    while at + 8 <= bytes.len() {
        let mut size = u32::from_be_bytes(bytes[at..at + 4].try_into().unwrap()) as u64;
        out.push(String::from_utf8_lossy(&bytes[at + 4..at + 8]).into_owned());
        if size == 1 {
            size = u64::from_be_bytes(bytes[at + 8..at + 16].try_into().unwrap());
        } else if size == 0 {
            break;
        }
        at += size as usize;
    }
    out
}

#[test]
fn encoder_choice_is_videotoolbox_on_mac_or_a_logged_fallback() {
    let path = out_dir().join("encoder_choice.mp4");
    let (mut w, warnings) = create_logged(&path, &settings());
    let name = w.encoder_name().to_owned();
    w.write_video(&tagged(0)).unwrap();
    w.finish().unwrap();
    let vt_fallback = warnings.iter().any(|m| m.contains("falling back"));
    let gpl_warning = warnings.iter().any(|m| m.contains("GPL"));
    println!(
        "encoder: {name}; VideoToolbox fallback warned: {vt_fallback}; warnings: {warnings:?}"
    );

    if cfg!(target_os = "macos") {
        assert!(
            name == "h264_videotoolbox" || vt_fallback,
            "{name} chosen without a VideoToolbox fallback warning"
        );
    } else {
        assert!(!vt_fallback, "no VideoToolbox off macOS");
        assert!(
            ["libopenh264", "libx264"].contains(&name.as_str()),
            "{name}"
        );
    }
    assert_eq!(name == "libx264", gpl_warning, "GPL warning iff libx264");
}

#[test]
fn video_only_export_is_valid() {
    let path = out_dir().join("video_only.mp4");
    write_standard(&path, false);
    let info = backend().probe(&path).unwrap();
    assert_eq!(info.video.expect("video").codec, "h264");
    assert_near_two_seconds(info.duration, "duration");
    assert_eq!(read_clip_tags(&path).len(), FRAMES);
}

#[test]
fn invalid_settings_are_rejected_before_touching_the_file() {
    let path = out_dir().join("invalid_settings.mp4");
    let _ = std::fs::remove_file(&path);
    let s = ExportSettings {
        width: 321,
        ..settings()
    };
    assert!(matches!(
        backend().create(&path, &s),
        Err(MediaError::InvalidExportSettings(_))
    ));
    assert!(!path.exists());
}

#[test]
fn unwritable_path_is_an_encode_error() {
    let path = out_dir().join("no-such-dir").join("out.mp4");
    match backend().create(&path, &settings()) {
        Err(MediaError::Encode { path: p, .. }) => assert_eq!(p, path),
        Err(e) => panic!("expected Encode error, got {e}"),
        Ok(_) => panic!("expected Encode error, got a writer"),
    }
}

#[test]
fn bad_input_is_an_error_not_a_panic() {
    let path = out_dir().join("bad_input.mp4");
    let mut w = backend().create(&path, &settings()).unwrap();
    let small = VideoFrame::solid(Micros(0), 160, 120, [0, 0, 0, 255]);
    assert!(matches!(
        w.write_video(&small),
        Err(MediaError::Encode { .. })
    ));
    w.write_video(&tagged(1)).unwrap();
    // Same frame time again, and a time before it.
    assert!(w.write_video(&tagged(1)).is_err());
    assert!(w.write_video(&tagged(0)).is_err());
    // Three samples is not a whole stereo frame.
    assert!(matches!(
        w.write_audio(&[0.0; 3]),
        Err(MediaError::Encode { .. })
    ));
    w.write_video(&tagged(2)).unwrap();
    w.finish().unwrap();
}

#[test]
fn dropping_an_unfinished_writer_does_not_panic() {
    let path = out_dir().join("dropped.mp4");
    let mut w = backend().create(&path, &settings()).unwrap();
    for i in 0..5 {
        w.write_video(&tagged(i)).unwrap();
    }
    w.write_audio(&sine(0, 5000)).unwrap();
    drop(w);
}
