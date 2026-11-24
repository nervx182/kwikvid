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
    let frame_plan = plan(edl, Rational::new(FPS, 1)).unwrap();
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
