//! End-to-end export: three tagged clips and a click track through the real
//! FFmpeg backend, then the MP4 is decoded and every frame identified by its
//! tag (`kv_testkit::read_clip_tags`).

use std::path::{Path, PathBuf};

use ffmpeg_next as ff;
use kv_core::{
    Edl, MediaId, MediaItem, MediaKind, Micros, MusicRef, Project, ProjectId, Segment, Speed,
    Transition,
};
use kv_media::{ExportSettings, FfmpegBackend, MediaBackend, Rational};
use kv_render::{export, plan, Progress};
use kv_testkit::{click_track, tagged_clip, ClipSpec, FrameTag};

const FPS: i32 = 30;
const RATE: u32 = 48_000;
/// Clip index (tag and hue) of media ids 1, 2 and 3: hues 120 degrees apart
/// so a blend of two clips never passes for either background.
const CLIPS: [u8; 3] = [0, 4, 8];

fn out_dir() -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("kv-render-export");
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn us(seconds: f64) -> Micros {
    Micros((seconds * 1e6).round() as i64)
}

fn seg(media: u64, src_in: f64, src_out: f64, transition: Transition) -> Segment {
    Segment {
        media_id: MediaId(media),
        src_in: us(src_in),
        src_out: us(src_out),
        timeline_start: Micros(0),
        transition,
        speed: Speed::NORMAL,
    }
}

/// `path` relative to `root`, `/`-separated, as a project stores it.
fn rel(root: &Path, path: &Path) -> String {
    let r = path.strip_prefix(root).expect("fixture under the root");
    r.components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("/")
}

/// 3 media items, a 120 BPM click track, and 4 segments: a cut, a 0.5 s
/// crossfade, a cut, and a cut back to the first clip.
fn project() -> (Project, PathBuf) {
    let music = click_track(120.0, 8.0, 44_100);
    let root = music.parent().unwrap().to_path_buf();
    let mut p = Project::new(ProjectId(1));
    for (i, &clip) in CLIPS.iter().enumerate() {
        let path = tagged_clip(&ClipSpec {
            clip,
            ..ClipSpec::default()
        });
        p.media.push(MediaItem {
            id: MediaId(i as u64 + 1),
            path: rel(&root, &path),
            duration: us(2.0),
            kind: MediaKind::Video,
            hilights: vec![],
            clip_audio: false,
            chapters: Vec::new(),
        });
    }
    let mut edl = Edl {
        segments: vec![
            seg(1, 0.0, 1.0, Transition::Cut),
            seg(2, 0.5, 1.5, Transition::Crossfade { duration: us(0.5) }),
            seg(3, 0.2, 1.2, Transition::Cut),
            seg(1, 1.0, 1.8, Transition::Cut),
        ],
    };
    edl.repack();
    let s = &mut p.sections[0];
    s.edit = edl;
    s.music = Some(MusicRef {
        path: rel(&root, &music),
        duration: us(8.0),
    });
    (p, root)
}

fn settings() -> ExportSettings {
    ExportSettings {
        width: 320,
        height: 180,
        fps: Rational::new(FPS, 1),
        video_bitrate: 2_000_000,
        audio_sample_rate: RATE,
        audio_channels: 2,
        ..ExportSettings::default()
    }
}

#[test]
fn exports_tagged_edit_with_crossfade_and_music() {
    let (project, root) = project();
    let backend = FfmpegBackend::new().unwrap();
    let out = out_dir().join("edit.mp4");
    let _ = std::fs::remove_file(&out);
    let mut progress: Vec<Progress> = Vec::new();
    let report = export(
        &project,
        Project::DEFAULT_SECTION,
        &root,
        &backend,
        &backend,
        &out,
        &settings(),
        &mut |p| progress.push(p),
    )
    .unwrap();
    println!("{report:?}");

    // 1 s + 1 s - 0.5 s overlap + 1 s + 0.8 s = 3.3 s = 99 frames.
    let edl = &project.sections[0].edit;
    let source_fps = |_: MediaId| Some(Rational::new(FPS, 1));
    let frame_plan = plan(edl, Rational::new(FPS, 1), &source_fps).unwrap();
    assert_eq!(frame_plan.len(), 99);
    assert_eq!(report.frames, 99);
    assert_eq!(report.duration, us(3.3));
    assert!(!report.encoder_name.is_empty());
    assert_eq!(
        progress.last().map(|p| (p.frame, p.total_frames)),
        Some((99, 99))
    );
    // Progress at least every 10 frames.
    let mut last = 0;
    for p in &progress {
        assert!(p.frame - last <= 10);
        last = p.frame;
    }

    let tags = kv_testkit::read_clip_tags(&out);
    assert_eq!(tags.len(), 99, "frame count");
    // Expected tag of a layer: its clip and the source frame at `src_time`.
    let expect = |l: &kv_render::Layer| {
        let clip = CLIPS[l.media_id.0 as usize - 1];
        let frame = (l.src_time.0 * i64::from(FPS) + 500_000) / 1_000_000;
        (clip, frame)
    };
    let matches = |tag: FrameTag, (clip, frame): (u8, i64)| {
        tag.clip == clip && (i64::from(tag.frame) - frame).abs() <= 1
    };
    let mut blended = Vec::new();
    for (f, tag) in frame_plan.iter().zip(&tags) {
        match f.layers.as_slice() {
            [only] => {
                let got = tag.unwrap_or_else(|| panic!("frame {} has no tag", f.index));
                assert!(
                    matches(got, expect(only)),
                    "frame {}: got {got:?}, want {:?}",
                    f.index,
                    expect(only)
                );
            }
            [out, inc] => {
                // A tag code only survives a blend when one clip dominates
                // (the reader's cells are black <= 1/3 and white >= 2/3 of
                // full scale): near the ends of the fade the frame may still
                // read as the dominant clip; in the middle it never does.
                let minority = out.weight.min(inc.weight);
                let dominant = if out.weight > inc.weight { out } else { inc };
                match tag {
                    None => blended.push(f.index),
                    Some(t) => {
                        assert!(
                            minority <= 0.3,
                            "frame {} (weights {} / {}) reads {t:?}",
                            f.index,
                            out.weight,
                            inc.weight
                        );
                        assert!(matches(*t, expect(dominant)), "frame {}: {t:?}", f.index);
                    }
                }
            }
            other => panic!("frame {} has layers {other:?}", f.index),
        }
    }
    // The overlap is [0.5 s, 1.0 s): frames 15..30, frame 15 at weight 0.
    // Untagged frames lie only inside it, and include its whole blended
    // middle (minority weight > 0.3: frames 20..=25; locally 19..=26 are).
    assert!(blended.iter().all(|k| (16..30).contains(k)), "{blended:?}");
    assert!((20..=25).all(|k| blended.contains(&k)), "{blended:?}");

    // Audio: AAC stereo at 48 kHz, 3.3 s (within one video frame).
    let info = backend.probe(&out).unwrap();
    let a = info.audio.expect("audio stream");
    assert_eq!(a.codec, "aac");
    assert_eq!(a.channels, 2);
    assert_eq!(a.sample_rate, RATE);
    let ictx = ff::format::input(&out).unwrap();
    let stream = ictx.streams().best(ff::media::Type::Audio).unwrap();
    let tb = stream.time_base();
    let dur = i128::from(stream.duration()) * 1_000_000 * i128::from(tb.numerator())
        / i128::from(tb.denominator());
    assert!(
        (dur - 3_300_000).abs() <= 1_000_000 / i128::from(FPS),
        "audio stream lasts {dur} us"
    );
}

/// A 30 fps tagged clip at speed 50 exported at 30 fps: even output frames
/// are source frames (clean tags, consecutive), odd ones 50/50 blends of
/// the source frames on either side (unreadable tags).
#[test]
fn slow_motion_blends_in_between_frames() {
    const CLIP: u8 = 6;
    let music = click_track(120.0, 8.0, 44_100);
    let root = music.parent().unwrap().to_path_buf();
    let path = tagged_clip(&ClipSpec {
        clip: CLIP,
        ..ClipSpec::default()
    });
    let mut p = Project::new(ProjectId(1));
    p.media.push(MediaItem {
        id: MediaId(1),
        path: rel(&root, &path),
        duration: us(2.0),
        kind: MediaKind::Video,
        hilights: vec![],
        clip_audio: false,
        chapters: Vec::new(),
    });
    // Source frames 6..36 (0.2 s .. 1.2 s) over 2 s.
    let mut edl = Edl {
        segments: vec![Segment {
            speed: Speed(50),
            ..seg(1, 0.2, 1.2, Transition::Cut)
        }],
    };
    edl.repack();
    p.sections[0].edit = edl;

    let backend = FfmpegBackend::new().unwrap();
    let out = out_dir().join("slowmo.mp4");
    let _ = std::fs::remove_file(&out);
    let report = export(
        &p,
        Project::DEFAULT_SECTION,
        &root,
        &backend,
        &backend,
        &out,
        &settings(),
        &mut |_| {},
    )
    .unwrap();
    assert_eq!(report.frames, 60);

    let source_fps = |_: MediaId| Some(Rational::new(FPS, 1));
    let frame_plan = plan(&p.sections[0].edit, Rational::new(FPS, 1), &source_fps).unwrap();
    let tags = kv_testkit::read_clip_tags(&out);
    assert_eq!(tags.len(), 60, "frame count");
    let src_frame = |t: Micros| (t.0 * i64::from(FPS) + 500_000) / 1_000_000;
    for (f, tag) in frame_plan.iter().zip(&tags) {
        let k = f.index as usize;
        let j = 6 + k / 2; // source frame at or before the exact time
        if k % 2 == 0 || k == 59 {
            // On a source frame (frame 59 holds the last frame before
            // src_out): a clean tag of exactly that frame.
            assert_eq!(f.layers.len(), 1, "frame {k}");
            assert_eq!(src_frame(f.layers[0].src_time), j as i64, "frame {k}");
            assert_eq!(
                *tag,
                Some(FrameTag {
                    clip: CLIP,
                    frame: j as u16
                }),
                "frame {k}"
            );
        } else {
            // In between: frames j and j + 1 at half weight each, and the
            // neighbours are exactly those frames.
            let layers: Vec<(i64, f32)> = f
                .layers
                .iter()
                .map(|l| (src_frame(l.src_time), l.weight))
                .collect();
            assert_eq!(layers.len(), 2, "frame {k}: {layers:?}");
            assert_eq!((layers[0].0, layers[1].0), (j as i64, j as i64 + 1));
            assert!(
                layers.iter().all(|l| (l.1 - 0.5).abs() < 1e-3),
                "{layers:?}"
            );
            assert_eq!(*tag, None, "frame {k} is a blend, read {tag:?}");
            let frame_of = |t: &Option<FrameTag>| t.map(|t| t.frame as usize);
            assert_eq!(frame_of(&tags[k - 1]), Some(j), "before frame {k}");
            assert_eq!(frame_of(&tags[k + 1]), Some(j + 1), "after frame {k}");
        }
    }
}

#[test]
fn missing_source_fails_before_creating_the_output() {
    let (mut project, root) = project();
    project.media[2].path = "does/not/exist.mp4".into();
    let backend = FfmpegBackend::new().unwrap();
    let out = out_dir().join("never.mp4");
    let _ = std::fs::remove_file(&out);
    let err = export(
        &project,
        Project::DEFAULT_SECTION,
        &root,
        &backend,
        &backend,
        &out,
        &settings(),
        &mut |_| {},
    )
    .unwrap_err();
    assert!(
        matches!(
            err,
            kv_render::RenderError::Source {
                media_id: MediaId(3),
                ..
            }
        ),
        "{err}"
    );
    assert!(!out.exists());
}

/// Every frame of an MP4, decoded to RGBA.
fn decode_all(backend: &FfmpegBackend, path: &Path) -> Vec<kv_media::VideoFrame> {
    let mut r = backend
        .open_video(path, &kv_media::DecodeOptions::default())
        .unwrap();
    let mut frames = Vec::new();
    while let Some(f) = r.next_frame().unwrap() {
        frames.push(f);
    }
    frames
}

/// A 1 s title over a 1.5 s clip: exported with and without the title,
/// the titled MP4 has a bright text region in the centre of the opening
/// frames and none after the title ends.
#[test]
fn title_card_is_drawn_over_the_opening_frames() {
    let music = click_track(120.0, 4.0, 44_100);
    let root = music.parent().unwrap().to_path_buf();
    let path = tagged_clip(&ClipSpec {
        clip: 2,
        ..ClipSpec::default()
    });
    let mut p = Project::new(ProjectId(1));
    p.media.push(MediaItem {
        id: MediaId(1),
        path: rel(&root, &path),
        duration: us(2.0),
        kind: MediaKind::Video,
        hilights: vec![],
        clip_audio: false,
        chapters: Vec::new(),
    });
    let mut edl = Edl {
        segments: vec![seg(1, 0.0, 1.5, Transition::Cut)],
    };
    edl.repack();
    p.sections[0].edit = edl;

    let backend = FfmpegBackend::new().unwrap();
    let run = |p: &Project, name: &str| {
        let out = out_dir().join(name);
        let _ = std::fs::remove_file(&out);
        let report = export(
            p,
            Project::DEFAULT_SECTION,
            &root,
            &backend,
            &backend,
            &out,
            &settings(),
            &mut |_| {},
        )
        .unwrap();
        assert_eq!(report.frames, 45);
        decode_all(&backend, &out)
    };
    let plain = run(&p, "untitled.mp4");
    p.sections[0].title = Some(kv_core::TitleCard {
        text: "TITLE".into(),
        duration: us(1.0),
    });
    let titled = run(&p, "titled.mp4");
    assert_eq!((plain.len(), titled.len()), (45, 45));

    // Near-white pixels in the central band (rows 70..110 of 180, the
    // middle 80 % of the width), where the title sits.
    let bright = |f: &kv_media::VideoFrame| {
        let w = f.width as usize;
        let mut n = 0usize;
        for y in 70..110usize {
            for x in 32..288usize {
                let i = (y * w + x) * 4;
                if f.data[i..i + 3].iter().all(|&c| c >= 220) {
                    n += 1;
                }
            }
        }
        n
    };
    let added: Vec<i64> = titled
        .iter()
        .zip(&plain)
        .map(|(t, u)| bright(t) as i64 - bright(u) as i64)
        .collect();
    println!("bright pixels added per frame: {added:?}");
    // Fully shown from 0.3 s to 0.7 s (frames 9..=21): a clear text region.
    assert!(added[9..=21].iter().all(|&n| n > 30), "{added:?}");
    // After 1 s (frame 30) plus a frame of encoder slack: no text.
    assert!(added[31..].iter().all(|&n| n.abs() < 30), "{added:?}");
}
