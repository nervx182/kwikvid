//! Deterministic engine tests: a fake media backend that renders tagged
//! frames (`kv_testkit`), the CPU compositor, a recording sink and a
//! [`ManualClock`].

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex as StdMutex;

use kv_audio::{render_music_bed, MusicBedOptions, PcmAudio};
use kv_core::{MediaItem, MediaKind, MusicRef, ProjectId, Segment, Speed, Transition};

use kv_media::{MediaInfo, VideoInfo};
use kv_testkit::{read_frame_tag, render_tagged_frame, FrameTag};

use super::*;

const W: u32 = 320;
const H: u32 = 180;
const FPS: Rational = Rational::new(30, 1);
/// Generous: the tests must not depend on machine speed.
const WAIT: Duration = Duration::from_secs(10);

fn pts(k: u64) -> Micros {
    Micros(frame_pts(k, FPS))
}

/// Index of the source frame shown at `t` in a 30 fps clip.
fn src_index(t: Micros) -> u16 {
    let mut j = (i128::from(t.0) * 30 / 1_000_000) as u64;
    while frame_pts(j + 1, FPS) <= t.0 {
        j += 1;
    }
    j as u16
}

type Hook = Arc<dyn Fn(u64) + Send + Sync>;

/// Media `clipN.mp4` decodes to tagged frames of clip `N`.
#[derive(Default)]
struct FakeMedia {
    opens: StdMutex<Vec<(PathBuf, DecodeOptions)>>,
    decodes: Arc<AtomicU64>,
    /// Runs before each decode with the decode count (stall, gate, ...).
    hook: Option<Hook>,
}

impl MediaBackend for FakeMedia {
    fn probe(&self, _: &Path) -> kv_media::Result<MediaInfo> {
        Ok(MediaInfo {
            duration: Micros(60_000_000),
            container: "mp4".into(),
            video: Some(VideoInfo {
                codec: "fake".into(),
                width: 640,
                height: 360,
                fps: FPS,
                bit_depth: 8,
                rotation: 0,
            }),
            audio: None,
            creation_time: None,
        })
    }

    fn open_video(
        &self,
        path: &Path,
        opts: &DecodeOptions,
    ) -> kv_media::Result<Box<dyn VideoReader>> {
        lock(&self.opens).push((path.to_path_buf(), *opts));
        let name = path.file_stem().unwrap().to_str().unwrap();
        let clip = name.trim_start_matches("clip").parse().unwrap();
        let (w, h) = opts.size.unwrap_or((640, 360));
        Ok(Box::new(FakeReader {
            clip,
            w,
            h,
            info: self.probe(path)?.video.unwrap(),
            decodes: Arc::clone(&self.decodes),
            hook: self.hook.clone(),
        }))
    }
}

struct FakeReader {
    clip: u8,
    w: u32,
    h: u32,
    info: VideoInfo,
    decodes: Arc<AtomicU64>,
    hook: Option<Hook>,
}

impl VideoReader for FakeReader {
    fn info(&self) -> &VideoInfo {
        &self.info
    }
    fn seek(&mut self, _: Micros) -> kv_media::Result<()> {
        Ok(())
    }
    fn next_frame(&mut self) -> kv_media::Result<Option<VideoFrame>> {
        self.frame_at(Micros(0))
    }
    fn frame_at(&mut self, t: Micros) -> kv_media::Result<Option<VideoFrame>> {
        let n = self.decodes.fetch_add(1, Ordering::SeqCst);
        if let Some(h) = &self.hook {
            h(n);
        }
        let frame = src_index(t);
        let data = render_tagged_frame(
            FrameTag {
                clip: self.clip,
                frame,
            },
            self.w,
            self.h,
        );
        Ok(Some(
            VideoFrame::new_rgba(pts(u64::from(frame)), self.w, self.h, data).unwrap(),
        ))
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct Shown {
    pts: Micros,
    tag: Option<FrameTag>,
    /// Clock time when presented.
    clock: Micros,
    size: (u32, u32),
}

#[derive(Clone)]
struct Recorder {
    shown: Arc<StdMutex<Vec<Shown>>>,
    clock: Arc<ManualClock>,
}

impl FrameSink for Recorder {
    fn present(&mut self, frame: &RenderedFrame<'_>) {
        let tag = match frame {
            RenderedFrame::Cpu(f) => read_frame_tag(&f.data, f.width, f.height),
            #[cfg(feature = "gpu")]
            RenderedFrame::Gpu { .. } => None,
        };
        lock(&self.shown).push(Shown {
            pts: frame.pts(),
            tag,
            clock: self.clock.now(),
            size: frame.size(),
        });
    }
}

fn media_item(id: u64) -> MediaItem {
    MediaItem {
        id: MediaId(id),
        path: format!("clips/clip{id}.mp4"),
        duration: Micros(60_000_000),
        kind: MediaKind::Video,
        hilights: vec![],
        clip_audio: false,
        chapters: Vec::new(),
    }
}

fn cut(media: u64, src_in: i64, len: i64) -> Segment {
    Segment {
        media_id: MediaId(media),
        src_in: Micros(src_in),
        src_out: Micros(src_in + len),
        timeline_start: Micros(0),
        transition: Transition::Cut,
        speed: Speed::NORMAL,
    }
}

fn project(segments: Vec<Segment>) -> Project {
    let mut p = Project::new(ProjectId(1));
    p.media = (1..=3).map(media_item).collect();
    let mut edl = Edl { segments };
    edl.repack();
    p.sections[0].edit = edl;
    p
}

/// Three cuts, 2 s in all: clip 1 from 1 s (0.5 s), clip 2 from 0 (1 s),
/// clip 1 again from 3 s (0.5 s).
fn three_segments() -> Project {
    project(vec![
        cut(1, 1_000_000, 500_000),
        cut(2, 0, 1_000_000),
        cut(1, 3_000_000, 500_000),
    ])
}

/// The tag output frame `k` of [`three_segments`] shows, worked out by hand.
fn expected(k: u64) -> FrameTag {
    let k16 = k as u16;
    match k {
        0..=14 => FrameTag {
            clip: 1,
            frame: 30 + k16,
        },
        15..=44 => FrameTag {
            clip: 2,
            frame: k16 - 15,
        },
        _ => FrameTag {
            clip: 1,
            frame: 90 + k16 - 45,
        },
    }
}

fn index_of(s: &Shown) -> u64 {
    index_at(FPS, s.pts, u64::MAX).unwrap()
}

struct Rig {
    engine: PreviewEngine,
    clock: Arc<ManualClock>,
    shown: Arc<StdMutex<Vec<Shown>>>,
    media: Arc<FakeMedia>,
}

impl Rig {
    fn new(media: FakeMedia, threads: usize) -> Rig {
        Rig::with_clock(media, threads, Arc::new(ManualClock::new()))
    }

    fn with_clock(media: FakeMedia, threads: usize, clock: Arc<ManualClock>) -> Rig {
        let media = Arc::new(media);
        let shown = Arc::new(StdMutex::new(Vec::new()));
        let sink = Recorder {
            shown: Arc::clone(&shown),
            clock: Arc::clone(&clock),
        };
        let config = PreviewConfig {
            fps: FPS,
            media_root: PathBuf::from("/media"),
            decode_threads: threads,
            max_wait: WAIT,
            clock: clock.clone(),
            ..PreviewConfig::default()
        };
        let engine = PreviewEngine::new(
            media.clone(),
            Box::new(CpuCompositor::new(W, H).unwrap()),
            Box::new(sink),
            config,
        )
        .unwrap();
        Rig {
            engine,
            clock,
            shown,
            media,
        }
    }

    fn shown(&self) -> Vec<Shown> {
        lock(&self.shown).clone()
    }

    fn last(&self) -> Shown {
        *self.shown().last().expect("a frame was shown")
    }

    /// Tick until something is presented (paused, so it is the due frame).
    fn present(&self) -> u64 {
        for _ in 0..100 {
            if let Tick::Presented(k) = self.engine.tick().unwrap() {
                return k;
            }
        }
        panic!("nothing presented");
    }
}

#[test]
fn plays_two_seconds_as_sixty_tagged_frames_in_order() {
    let rig = Rig::new(FakeMedia::default(), 2);
    rig.engine
        .set_edit(&three_segments(), Project::DEFAULT_SECTION)
        .unwrap();
    rig.engine.play();
    for k in 0..60 {
        rig.clock.set(pts(k));
        assert_eq!(rig.engine.tick().unwrap(), Tick::Presented(k));
    }
    let shown = rig.shown();
    assert_eq!(shown.len(), 60);
    for (k, s) in shown.iter().enumerate() {
        assert_eq!(s.pts, pts(k as u64));
        assert_eq!(s.tag, Some(expected(k as u64)), "frame {k}");
        assert_eq!(s.size, (W, H));
    }
    // Past the end: playback stops on the last frame.
    rig.clock.set(Micros(2_100_000));
    assert_eq!(rig.engine.tick().unwrap(), Tick::Unchanged);
    assert!(!rig.engine.is_playing());
    assert_eq!(rig.engine.position(), Micros(2_000_000));

    let stats = rig.engine.stats();
    assert_eq!((stats.presented, stats.dropped), (60, 0));
    // One reader per file, reused by both clip 1 segments, opened at the
    // size the 320x180 compositor needs for a 640x360 source.
    let opens = lock(&rig.media.opens).clone();
    assert_eq!(opens.len(), 2, "{opens:?}");
    assert_eq!(stats.readers_opened, 2);
    for (path, opts) in opens {
        assert!(path.starts_with("/media/clips"), "{path:?}");
        assert_eq!(opts.size, Some((W, H)));
        assert!(opts.apply_rotation);
    }
}

#[test]
fn seek_shows_the_target_frame_first() {
    let rig = Rig::new(FakeMedia::default(), 2);
    let reported = Arc::new(StdMutex::new(Vec::new()));
    let r = Arc::clone(&reported);
    rig.engine.on_position(move |t| lock(&r).push(t));
    rig.engine
        .set_edit(&three_segments(), Project::DEFAULT_SECTION)
        .unwrap();
    assert_eq!(rig.present(), 0);

    // Paused: forwards, backwards, forwards.
    for (t, k) in [(1_200_000, 36), (300_000, 9), (1_900_000, 57)] {
        rig.engine.seek(Micros(t));
        assert_eq!(rig.engine.position(), Micros(t));
        let before = rig.shown().len();
        assert_eq!(rig.present(), k);
        let first = rig.shown()[before];
        assert_eq!(first.tag, Some(expected(k)), "seek to {t}");
        assert!(rig.engine.stats().last_seek_latency.is_some());
        assert!(lock(&reported).contains(&Micros(t)));
    }

    // Playing: a backwards seek shows its frame next, then plays on.
    rig.engine.seek(Micros(1_500_000));
    rig.clock.set(Micros(5_000_000));
    rig.engine.play();
    assert_eq!(rig.present(), 45);
    rig.clock.advance(pts(1));
    assert_eq!(rig.engine.tick().unwrap(), Tick::Presented(46));
    rig.engine.seek(Micros(500_000));
    assert_eq!(rig.engine.tick().unwrap(), Tick::Presented(15));
    assert_eq!(rig.last().tag, Some(expected(15)));
    rig.clock.advance(pts(2));
    assert_eq!(rig.engine.tick().unwrap(), Tick::Presented(17));
    assert_eq!(rig.last().tag, Some(expected(17)));
}

/// Item 1 as two 1 s chapter files, `clip1.mp4` then `clip2.mp4`, cut from
/// 0.5 s to 1.5 s: across the boundary.
fn chaptered_project() -> Project {
    let mut p = project(vec![cut(1, 500_000, 1_000_000)]);
    p.media[0].duration = Micros(2_000_000);
    p.media[0].chapters = ["clips/clip1.mp4", "clips/clip2.mp4"]
        .map(|path| kv_core::ChapterRef {
            path: path.into(),
            duration: Micros(1_000_000),
        })
        .to_vec();
    p
}

#[test]
fn a_chaptered_item_plays_across_its_chapter_files() {
    let rig = Rig::new(FakeMedia::default(), 1);
    rig.engine
        .set_edit(&chaptered_project(), Project::DEFAULT_SECTION)
        .unwrap();
    rig.engine.play();
    for k in 0..30 {
        rig.clock.set(pts(k));
        assert_eq!(rig.engine.tick().unwrap(), Tick::Presented(k));
    }
    let tags: Vec<_> = rig.shown().iter().map(|s| s.tag).collect();
    let want: Vec<_> = (15..30u16)
        .map(|frame| Some(FrameTag { clip: 1, frame }))
        .chain((0..15u16).map(|frame| Some(FrameTag { clip: 2, frame })))
        .collect();
    assert_eq!(tags, want);
    let opened: Vec<PathBuf> = lock(&rig.media.opens).iter().map(|o| o.0.clone()).collect();
    assert_eq!(
        opened,
        [
            PathBuf::from("/media/clips/clip1.mp4"),
            PathBuf::from("/media/clips/clip2.mp4")
        ]
    );

    // The same item without chapters: its reader is reopened as a plain
    // single-file one, so the second half shows clip 1 again.
    let mut plain = chaptered_project();
    plain.media[0].chapters.clear();
    rig.engine.pause();
    rig.engine
        .set_edit(&plain, Project::DEFAULT_SECTION)
        .unwrap();
    rig.engine.seek(pts(20));
    assert_eq!(rig.present(), 20);
    assert_eq!(rig.last().tag, Some(FrameTag { clip: 1, frame: 35 }));
}

#[test]
fn seek_cancels_queued_prefetch() {
    // Decoders block until the gate opens, so prefetch piles up.
    let gate = Arc::new((StdMutex::new(false), Condvar::new()));
    let g = Arc::clone(&gate);
    let media = FakeMedia {
        hook: Some(Arc::new(move |_| {
            let (open, cv) = &*g;
            let mut o = lock(open);
            while !*o {
                o = cv.wait(o).unwrap();
            }
        })),
        ..FakeMedia::default()
    };
    let rig = Rig::new(media, 1);
    rig.engine
        .set_edit(&three_segments(), Project::DEFAULT_SECTION)
        .unwrap();
    rig.engine.play();
    assert_eq!(rig.engine.stats().cancelled, 0);
    rig.engine.seek(Micros(1_000_000));
    // 15 frames of prefetch were queued, one is being decoded.
    assert!(
        rig.engine.stats().cancelled >= 10,
        "{:?}",
        rig.engine.stats()
    );
    {
        let (open, cv) = &*gate;
        *lock(open) = true;
        cv.notify_all();
    }
    assert_eq!(rig.present(), 30);
    assert_eq!(rig.shown().len(), 1, "nothing from before the seek");
    assert_eq!(rig.last().tag, Some(expected(30)));
}

#[test]
fn slow_decoder_drops_frames_instead_of_drifting() {
    // Every 20th decode stalls for 150 ms (4.5 frames) of clock time.
    let clock = Arc::new(ManualClock::new());
    let c = Arc::clone(&clock);
    let media = FakeMedia {
        hook: Some(Arc::new(move |n| {
            if n % 20 == 10 {
                c.advance(Micros(150_000));
            }
        })),
        ..FakeMedia::default()
    };
    let rig = Rig::with_clock(media, 1, clock);
    rig.engine
        .set_edit(&three_segments(), Project::DEFAULT_SECTION)
        .unwrap();
    rig.engine.play();
    while rig.engine.is_playing() {
        rig.engine.tick().unwrap();
        rig.clock.advance(pts(1));
    }
    // Stopped at the end: the last frame is shown.
    rig.engine.tick().unwrap();

    let shown = rig.shown();
    let idx: Vec<u64> = shown.iter().map(index_of).collect();
    assert!(idx.windows(2).all(|w| w[0] < w[1]), "{idx:?}");
    assert_eq!(*idx.last().unwrap(), 59, "{idx:?}");
    for s in &shown {
        let k = index_of(s);
        assert_eq!(s.tag, Some(expected(k)), "frame {k}");
        // Never shown before it is due.
        assert!(index_at(FPS, s.clock, 60).unwrap() >= k);
    }
    let stats = rig.engine.stats();
    assert_eq!(stats.presented, shown.len() as u64);
    assert_eq!(stats.presented + stats.dropped, 60, "{stats:?}");
    assert!(stats.dropped >= 3, "{stats:?} {idx:?}");
    // Playback ended when the clock reached the end, not later.
    assert_eq!(rig.engine.position(), Micros(2_000_000));
}

#[test]
fn editing_while_playing_keeps_the_position() {
    let rig = Rig::new(FakeMedia::default(), 2);
    rig.engine
        .set_edit(&three_segments(), Project::DEFAULT_SECTION)
        .unwrap();
    rig.engine.play();
    for k in 0..=20 {
        rig.clock.set(pts(k));
        assert_eq!(rig.engine.tick().unwrap(), Tick::Presented(k));
    }
    // New snapshot: clip 3 throughout.
    rig.engine
        .set_edit(
            &project(vec![cut(3, 0, 2_000_000)]),
            Project::DEFAULT_SECTION,
        )
        .unwrap();
    assert!(rig.engine.is_playing());
    assert_eq!(rig.engine.position(), pts(20));
    assert_eq!(rig.engine.tick().unwrap(), Tick::Presented(20));
    assert_eq!(rig.last().tag, Some(FrameTag { clip: 3, frame: 20 }));
    rig.clock.set(pts(21));
    assert_eq!(rig.engine.tick().unwrap(), Tick::Presented(21));
    assert_eq!(rig.engine.stats().dropped, 0);
}

#[test]
fn unknown_section_and_empty_edit() {
    let rig = Rig::new(FakeMedia::default(), 1);
    assert_eq!(rig.engine.tick().unwrap(), Tick::Idle);
    assert_eq!(
        rig.engine.set_edit(&three_segments(), SectionId(9)),
        Err(PreviewError::UnknownSection(SectionId(9)))
    );
    rig.engine
        .set_edit(&project(vec![]), Project::DEFAULT_SECTION)
        .unwrap();
    rig.engine.play();
    assert!(!rig.engine.is_playing());
    assert_eq!(rig.engine.tick().unwrap(), Tick::Idle);
}

#[test]
fn media_missing_from_the_project_draws_black() {
    let rig = Rig::new(FakeMedia::default(), 1);
    rig.engine
        .set_edit(
            &project(vec![cut(7, 0, 1_000_000)]),
            Project::DEFAULT_SECTION,
        )
        .unwrap();
    assert_eq!(rig.present(), 0);
    let s = rig.last();
    assert_eq!(s.tag, None);
}

#[test]
fn pacer_plays_in_real_time() {
    let media = Arc::new(FakeMedia::default());
    let shown = Arc::new(StdMutex::new(Vec::new()));
    let sink = Recorder {
        shown: Arc::clone(&shown),
        clock: Arc::new(ManualClock::new()),
    };
    let engine = Arc::new(
        PreviewEngine::new(
            media,
            Box::new(CpuCompositor::new(W, H).unwrap()),
            Box::new(sink),
            PreviewConfig::default(),
        )
        .unwrap(),
    );
    engine
        .set_edit(&project(vec![cut(1, 0, 300_000)]), Project::DEFAULT_SECTION)
        .unwrap();
    engine.play();
    let pacer = Pacer::spawn(Arc::clone(&engine));
    let start = Instant::now();
    while (engine.is_playing() || lock(&shown).last().map(|s| s.pts) != Some(pts(8)))
        && start.elapsed() < Duration::from_secs(20)
    {
        std::thread::sleep(Duration::from_millis(5));
    }
    drop(pacer);
    let idx: Vec<u64> = lock(&shown).iter().map(index_of).collect();
    assert_eq!(idx.last(), Some(&8), "{idx:?}");
    assert!(idx.windows(2).all(|w| w[0] < w[1]), "{idx:?}");
}

#[test]
fn preview_size_uses_output_or_proxy() {
    // Fits in the viewer: render at output size.
    assert_eq!(preview_size((1280, 720), (1440, 810)), (1280, 720));
    // Larger than the viewer: proxy, keeping the shape.
    assert_eq!(preview_size((1920, 1080), (1440, 810)), (960, 540));
    assert_eq!(preview_size((3840, 2160), (800, 450)), (960, 540));
    assert_eq!(preview_size((1080, 1920), (400, 700)), (304, 540));
    assert_eq!(preview_size((1080, 1080), (500, 500)), (540, 540));
    // Already proxy-sized or smaller: never upscaled.
    assert_eq!(preview_size((640, 360), (320, 180)), (640, 360));
}

#[test]
fn frame_index_and_window() {
    assert_eq!(index_at(FPS, Micros(0), 0), None);
    assert_eq!(index_at(FPS, Micros(0), 60), Some(0));
    assert_eq!(index_at(FPS, Micros(33_332), 60), Some(0));
    assert_eq!(index_at(FPS, Micros(33_333), 60), Some(1));
    assert_eq!(index_at(FPS, Micros(-5), 60), Some(0));
    assert_eq!(index_at(FPS, Micros(9_000_000), 60), Some(59));
    let ntsc = Rational::new(30000, 1001);
    for k in [0u64, 1, 29, 30, 1000, 17_982] {
        let t = Micros(frame_pts(k, ntsc));
        assert_eq!(index_at(ntsc, t, 20_000), Some(k));
        assert_eq!(
            index_at(ntsc, Micros(t.0 - 1), 20_000),
            Some(k.saturating_sub(1))
        );
    }
    assert_eq!(window_frames(Micros(500_000), FPS), 15);
    assert_eq!(window_frames(Micros(500_001), FPS), 16);
    assert_eq!(window_frames(Micros(0), FPS), 1);
    assert_eq!(
        resolve(Path::new("/p"), "a/b.mp4"),
        PathBuf::from("/p/a/b.mp4")
    );
    assert_eq!(
        resolve(Path::new("/p"), "/abs/c.mp4"),
        PathBuf::from("/abs/c.mp4")
    );
}

#[test]
fn invalid_config_is_rejected() {
    let media: Arc<dyn MediaBackend> = Arc::new(FakeMedia::default());
    let sink = || -> Box<dyn FrameSink> {
        Box::new(Recorder {
            shown: Arc::default(),
            clock: Arc::new(ManualClock::new()),
        })
    };
    let bad_fps = PreviewConfig {
        fps: Rational::new(0, 1),
        ..PreviewConfig::default()
    };
    let cpu = Box::new(CpuCompositor::new(W, H).unwrap());
    assert!(matches!(
        PreviewEngine::new(media, cpu, sink(), bad_fps),
        Err(PreviewError::Plan(_))
    ));
    assert!(matches!(
        CpuCompositor::new(0, 10),
        Err(PreviewError::InvalidSize { .. })
    ));
}

#[cfg(feature = "gpu")]
#[test]
fn gpu_compositor_presents_textures() {
    use crate::gpu::{GpuCompositor, GpuContext, WgpuContext};
    let ctx = match WgpuContext::headless() {
        Ok(c) => c,
        Err(e) => {
            println!("SKIPPED: no GPU adapter ({e})");
            return;
        }
    };
    let clock = Arc::new(ManualClock::new());
    let shown = Arc::new(StdMutex::new(Vec::new()));
    let sink = Recorder {
        shown: Arc::clone(&shown),
        clock: Arc::clone(&clock),
    };
    let engine = PreviewEngine::new(
        Arc::new(FakeMedia::default()),
        Box::new(GpuCompositor::new(&ctx, W, H).unwrap()),
        Box::new(sink),
        PreviewConfig {
            max_wait: WAIT,
            clock: clock.clone(),
            ..PreviewConfig::default()
        },
    )
    .unwrap();
    engine
        .set_edit(&three_segments(), Project::DEFAULT_SECTION)
        .unwrap();
    engine.seek(Micros(1_000_000));
    assert_eq!(engine.tick().unwrap(), Tick::Presented(30));
    engine.play();
    clock.advance(pts(1));
    assert_eq!(engine.tick().unwrap(), Tick::Presented(31));
    let shown = lock(&shown).clone();
    assert_eq!(shown.len(), 2);
    assert_eq!(shown[0].pts, pts(30));
    assert!(shown.iter().all(|s| s.size == (W, H)));
}

// ---- Title card: drawn over the opening frames, like export.

/// Keeps every presented CPU frame.
#[derive(Clone, Default)]
struct FrameRecorder(Arc<StdMutex<Vec<VideoFrame>>>);

impl FrameSink for FrameRecorder {
    fn present(&mut self, frame: &RenderedFrame<'_>) {
        if let RenderedFrame::Cpu(f) = frame {
            lock(&self.0).push((*f).clone());
        }
    }
}

/// A [`CpuCompositor`] counting the overlay images it is given.
struct CountingCompositor {
    inner: CpuCompositor,
    sets: Arc<AtomicU64>,
}

impl PreviewCompositor for CountingCompositor {
    fn size(&self) -> (u32, u32) {
        self.inner.size()
    }
    fn set_overlay(
        &mut self,
        id: OverlayId,
        image: Option<&VideoFrame>,
    ) -> Result<(), PreviewError> {
        self.sets.fetch_add(1, Ordering::SeqCst);
        self.inner.set_overlay(id, image)
    }
    fn render_with_overlays(
        &mut self,
        pts: Micros,
        layers: &[(&VideoFrame, f32)],
        overlays: &[Overlay],
        fit: Fit,
    ) -> Result<RenderedFrame<'_>, PreviewError> {
        self.inner.render_with_overlays(pts, layers, overlays, fit)
    }
}

struct TitleRig {
    engine: PreviewEngine,
    clock: Arc<ManualClock>,
    frames: FrameRecorder,
    sets: Arc<AtomicU64>,
}

impl TitleRig {
    fn new() -> TitleRig {
        let clock = Arc::new(ManualClock::new());
        let frames = FrameRecorder::default();
        let sets = Arc::new(AtomicU64::new(0));
        let engine = PreviewEngine::new(
            Arc::new(FakeMedia::default()),
            Box::new(CountingCompositor {
                inner: CpuCompositor::new(W, H).unwrap(),
                sets: Arc::clone(&sets),
            }),
            Box::new(frames.clone()),
            PreviewConfig {
                fps: FPS,
                media_root: PathBuf::from("/media"),
                decode_threads: 2,
                max_wait: WAIT,
                clock: clock.clone(),
                ..PreviewConfig::default()
            },
        )
        .unwrap();
        TitleRig {
            engine,
            clock,
            frames,
            sets,
        }
    }

    /// Show `p` and return the presented frame at index `k` (paused).
    fn frame(&self, p: &Project, k: u64) -> VideoFrame {
        self.engine.set_edit(p, Project::DEFAULT_SECTION).unwrap();
        self.engine.seek(pts(k));
        self.tick_presented(k)
    }

    fn tick_presented(&self, k: u64) -> VideoFrame {
        for _ in 0..100 {
            if let Tick::Presented(i) = self.engine.tick().unwrap() {
                assert_eq!(i, k);
                return lock(&self.frames.0).last().unwrap().clone();
            }
        }
        panic!("nothing presented");
    }

    fn sets(&self) -> u64 {
        self.sets.load(Ordering::SeqCst)
    }
}

/// [`three_segments`] with a 1 s title `text` (or none).
fn titled(text: Option<&str>) -> Project {
    let mut p = three_segments();
    p.sections[0].title = text.map(|t| TitleCard {
        text: t.into(),
        duration: Micros(1_000_000),
    });
    p
}

/// Centre pixels of `titled` that are bright (text) and differ from `plain`.
fn title_pixels(titled: &VideoFrame, plain: &VideoFrame) -> usize {
    let mut n = 0;
    for y in H as usize / 3..2 * H as usize / 3 {
        for x in W as usize / 10..9 * W as usize / 10 {
            let i = (y * W as usize + x) * 4;
            let (t, p) = (&titled.data[i..i + 3], &plain.data[i..i + 3]);
            if t.iter().all(|&c| c > 200) && t != p {
                n += 1;
            }
        }
    }
    n
}

#[test]
fn title_is_drawn_over_the_opening_frames_only() {
    let rig = TitleRig::new();
    // Frame 15 (0.5 s): fully shown; frame 45 (1.5 s): after the title.
    let with = [15, 45].map(|k| rig.frame(&titled(Some("Summer")), k));
    assert_eq!(rig.sets(), 1);
    // Same text, other duration: not rasterised again.
    let mut longer = titled(Some("Summer"));
    longer.sections[0].title.as_mut().unwrap().duration = Micros(2_000_000);
    rig.frame(&longer, 0);
    assert_eq!(rig.sets(), 1);
    let without = [15, 45].map(|k| rig.frame(&titled(None), k));
    assert_eq!(rig.sets(), 2);
    let n = title_pixels(&with[0], &without[0]);
    assert!(n > 40, "{n} title pixels");
    assert_eq!(with[1], without[1], "frames after the title are unchanged");
    // Frame 0 is at fade opacity 0: the footage, with its tag readable.
    let first = rig.frame(&titled(Some("Summer")), 0);
    assert_eq!(read_frame_tag(&first.data, W, H), Some(expected(0)));
    assert_eq!(rig.sets(), 3);
}

#[test]
fn changing_the_title_while_playing_updates_the_next_frame() {
    let rig = TitleRig::new();
    let plain = rig.frame(&titled(None), 15);
    assert_eq!(rig.sets(), 0);
    rig.engine
        .set_edit(&titled(Some("Summer")), Project::DEFAULT_SECTION)
        .unwrap();
    rig.engine.seek(Micros(0));
    rig.engine.play();
    for k in 0..15 {
        rig.clock.set(pts(k));
        assert_eq!(rig.engine.tick().unwrap(), Tick::Presented(k));
    }
    rig.clock.set(pts(15));
    let summer = rig.tick_presented(15);
    assert!(title_pixels(&summer, &plain) > 40);

    // New text: the same frame is redrawn with it on the next tick.
    rig.engine
        .set_edit(&titled(Some("Winter")), Project::DEFAULT_SECTION)
        .unwrap();
    let winter = rig.tick_presented(15);
    assert!(title_pixels(&winter, &plain) > 40);
    assert_ne!(winter, summer);

    // Cleared: back to the plain footage.
    rig.engine
        .set_edit(&titled(None), Project::DEFAULT_SECTION)
        .unwrap();
    assert_eq!(rig.tick_presented(15), plain);
    assert!(rig.engine.is_playing());
    assert_eq!(rig.sets(), 3);
}

#[cfg(feature = "gpu")]
#[test]
fn gpu_preview_title_matches_cpu() {
    use crate::gpu::{GpuCompositor, GpuContext, WgpuContext};
    let ctx = match WgpuContext::headless() {
        Ok(c) => c,
        Err(e) => {
            println!("SKIPPED: no GPU adapter ({e})");
            return;
        }
    };
    let footage = VideoFrame::new_rgba(
        Micros(0),
        W,
        H,
        render_tagged_frame(FrameTag { clip: 2, frame: 7 }, W, H),
    )
    .unwrap();
    let card = TitleCard {
        text: "Summer".into(),
        duration: Micros(1_000_000),
    };
    let image = title::rasterize(&card, W, H);
    let mut cpu = CpuCompositor::new(W, H).unwrap();
    let mut gpu = GpuCompositor::new(&ctx, W, H).unwrap();
    cpu.set_overlay(OverlayId::Title, Some(&image)).unwrap();
    gpu.set_overlay(OverlayId::Title, Some(&image)).unwrap();
    let textures = gpu.texture_count();
    for opacity in [1.0, 0.5, 0.0] {
        let overlays = [Overlay {
            id: OverlayId::Title,
            opacity,
        }];
        let layers = [(&footage, 1.0)];
        let want = match cpu
            .render_with_overlays(Micros(5), &layers, &overlays, Fit::Fill)
            .unwrap()
        {
            RenderedFrame::Cpu(f) => f.clone(),
            RenderedFrame::Gpu { .. } => unreachable!(),
        };
        PreviewCompositor::render_with_overlays(&mut gpu, Micros(5), &layers, &overlays, Fit::Fill)
            .unwrap();
        let got = gpu.read_back().unwrap();
        let max = got
            .data
            .iter()
            .zip(&want.data)
            .map(|(a, b)| a.abs_diff(*b))
            .max()
            .unwrap();
        assert!(max <= 3, "opacity {opacity}: max channel difference {max}");
        if opacity == 1.0 {
            assert!(title_pixels(&want, &footage) > 40);
        }
    }
    // The title texture was uploaded once: frames reuse it.
    assert_eq!(gpu.texture_count(), textures + 1);
    gpu.set_overlay(OverlayId::Title, None).unwrap();
    assert!(gpu.preview_overlays.is_empty());
}

// ---- Audio: the music bed plays in sync and the device clock drives video.

const AUDIO_RATE: u32 = 8_000;

struct AudioRig {
    engine: PreviewEngine,
    out: NullOutput,
    wall: Arc<ManualClock>,
    shown: Arc<StdMutex<Vec<Shown>>>,
    /// The bed the engine should be playing, rendered independently.
    bed: PcmAudio,
}

impl AudioRig {
    fn new(volume_percent: u8) -> AudioRig {
        let music = kv_testkit::click_track(120.0, 3.0, AUDIO_RATE);
        let mut project = three_segments();
        project.sections[0].music = Some(MusicRef {
            path: music.to_str().unwrap().to_string(),
            duration: Micros(3_000_000),
        });
        project.settings.music_volume_percent = volume_percent;
        let mut opts = MusicBedOptions::new(AUDIO_RATE, 1, Micros(2_000_000));
        opts.fade_out = crate::MUSIC_FADE_OUT;
        let bed = render_music_bed(&music, &opts).unwrap();
        AudioRig::build(&project, bed)
    }

    /// An engine with a hand-pulled mono device showing `project`; `bed`
    /// is what the test expects it to play.
    fn build(project: &Project, bed: PcmAudio) -> AudioRig {
        // The device: mono at 8 kHz, pulled by hand (see `pull`).
        let wall = Arc::new(ManualClock::new());
        let transport = AudioTransport::new(AUDIO_RATE, 1, wall.clone());
        let mut out = NullOutput::new(AUDIO_RATE, 1);
        out.start(Arc::clone(&transport)).unwrap();

        let shown = Arc::new(StdMutex::new(Vec::new()));
        let sink = Recorder {
            shown: Arc::clone(&shown),
            clock: wall.clone(),
        };
        let config = PreviewConfig {
            fps: FPS,
            media_root: PathBuf::from("/media"),
            decode_threads: 2,
            max_wait: WAIT,
            // Must be ignored in favour of the audio clock.
            clock: Arc::new(ManualClock::new()),
            audio: Some(transport),
            ..PreviewConfig::default()
        };
        let engine = PreviewEngine::new(
            Arc::new(FakeMedia::default()),
            Box::new(CpuCompositor::new(W, H).unwrap()),
            Box::new(sink),
            config,
        )
        .unwrap();
        engine.set_edit(project, Project::DEFAULT_SECTION).unwrap();
        assert!(engine.wait_for_audio(WAIT));
        AudioRig {
            engine,
            out,
            wall,
            shown,
            bed,
        }
    }

    /// The device plays `ms` milliseconds (no latency): it pulls the
    /// buffer, then that much wall time passes while it is heard, so the
    /// clock ends at the buffer's end.
    fn pull(&self, ms: usize) -> Vec<f32> {
        let got = self.out.pull(ms * AUDIO_RATE as usize / 1_000, Micros(0));
        self.wall.advance(Micros(ms as i64 * 1_000));
        got
    }

    fn bed_at(&self, ms: usize, len_ms: usize) -> &[f32] {
        let f = AUDIO_RATE as usize / 1_000;
        &self.bed.samples[ms * f..(ms + len_ms) * f]
    }

    fn tick_until_presented(&self) -> u64 {
        for _ in 0..100 {
            if let Tick::Presented(k) = self.engine.tick().unwrap() {
                return k;
            }
        }
        panic!("nothing presented");
    }
}

#[test]
fn audio_clock_drives_playback_and_plays_the_bed_from_the_playhead() {
    let rig = AudioRig::new(100);
    assert!(rig.bed.samples.iter().any(|s| s.abs() > 0.1), "clicks");
    // Paused: the device plays silence and the picture stays put.
    assert!(rig.pull(100).iter().all(|s| *s == 0.0));
    assert_eq!(rig.engine.position(), Micros(0));

    rig.engine.play();
    // Nothing moves until the device plays.
    assert_eq!(rig.engine.position(), Micros(0));
    assert_eq!(rig.tick_until_presented(), 0);
    // Each 100 ms the device plays is the next 100 ms of the bed, and the
    // frame due at that position is what gets shown.
    for step in 0..5 {
        let got = rig.pull(100);
        assert_eq!(got, rig.bed_at(step * 100, 100), "audio step {step}");
        let pos = Micros((step as i64 + 1) * 100_000);
        assert_eq!(rig.engine.position(), pos);
        let k = rig.tick_until_presented();
        assert_eq!(k, index_at(FPS, pos, u64::MAX).unwrap());
        assert_eq!(lock(&rig.shown).last().unwrap().tag, Some(expected(k)));
    }

    // Seek while playing: the next buffer is the bed at the target and the
    // picture follows from there.
    rig.engine.seek(Micros(1_200_000));
    assert_eq!(rig.engine.position(), Micros(1_200_000));
    assert_eq!(rig.pull(50), rig.bed_at(1_200, 50));
    assert_eq!(rig.engine.position(), Micros(1_250_000));
    assert_eq!(rig.tick_until_presented(), 37);

    // Pause: silence, and the position stays where it was.
    rig.engine.pause();
    assert!(rig.pull(100).iter().all(|s| *s == 0.0));
    assert_eq!(rig.engine.position(), Micros(1_250_000));

    // Seek while paused, then play: the bed resumes at the new playhead.
    rig.engine.seek(Micros(300_000));
    rig.engine.play();
    assert_eq!(rig.pull(20), rig.bed_at(300, 20));
    assert_eq!(rig.engine.position(), Micros(320_000));

    // Past the end the engine stops and so does the sound.
    rig.pull(2_000);
    rig.engine.tick().unwrap();
    assert!(!rig.engine.is_playing());
    assert!(rig.pull(50).iter().all(|s| *s == 0.0));
}

#[test]
fn audio_uses_the_project_music_volume() {
    let rig = AudioRig::new(50);
    rig.engine.play();
    let got = rig.pull(500);
    for (g, b) in got.iter().zip(rig.bed_at(0, 500)) {
        assert!((g - b * 0.5).abs() < 1e-6, "{g} vs {b}");
    }
}

// ---- Clip audio: mixed under the music as in export.

/// Amplitude of the tone in the clip fixtures.
const TONE: f32 = 0.05;

/// Amplitude of the `freq` Hz component of `x` (Hann-windowed single-bin
/// DFT, normalised so a sine of amplitude `a` gives `a`).
fn tone_level(x: &[f32], freq: f32, rate: u32) -> f32 {
    let n = x.len() as f64;
    let (mut re, mut im, mut wsum) = (0.0f64, 0.0f64, 0.0f64);
    for (i, &v) in x.iter().enumerate() {
        let w = 0.5 - 0.5 * (std::f64::consts::TAU * i as f64 / n).cos();
        let ph = f64::from(std::f32::consts::TAU * freq) * i as f64 / f64::from(rate);
        re += w * f64::from(v) * ph.cos();
        im += w * f64::from(v) * ph.sin();
        wsum += w;
    }
    (2.0 * (re * re + im * im).sqrt() / wsum) as f32
}

/// Media 1 and 2 are real 2 s clips with a 1 kHz tone track (the fake
/// video backend draws their frames). The edit, 2.5 s: media 1 for 1 s,
/// media 2 for 1 s, media 1 for 0.5 s. Media 1 has clip audio on when
/// `clip_audio`; media 2 never.
fn tone_project(clip_audio: bool) -> Project {
    let mut p = project(vec![
        cut(1, 200_000, 1_000_000),
        cut(2, 200_000, 1_000_000),
        cut(1, 1_000_000, 500_000),
    ]);
    for item in &mut p.media {
        let path = kv_testkit::tagged_clip(&kv_testkit::ClipSpec {
            clip: item.id.0 as u8,
            audio: true,
            audio_hz: 1_000,
            file_name: Some(format!("clip{}.mp4", item.id.0)),
            ..kv_testkit::ClipSpec::default()
        });
        item.path = path.to_str().unwrap().to_string();
    }
    p.media[0].clip_audio = clip_audio;
    p
}

const TONE_LEN: Micros = Micros(2_500_000);

/// What export mixes for [`tone_project`]: media 1's clip audio alone.
fn tone_clip_audio(project: &Project) -> PcmAudio {
    let sources = [kv_audio::ClipAudioSource {
        media_id: MediaId(1),
        path: project.media[0].path.clone().into(),
    }];
    let opts = kv_audio::ClipAudioOptions {
        sample_rate: AUDIO_RATE,
        channels: 1,
        length: TONE_LEN,
    };
    kv_audio::render_clip_audio(&project.sections[0].edit, &sources, &opts).unwrap()
}

fn ms(t: usize) -> usize {
    t * AUDIO_RATE as usize / 1_000
}

#[test]
fn clip_audio_plays_its_tone_exactly_inside_its_segments() {
    let mut project = tone_project(true);
    let rig = AudioRig::build(&project, tone_clip_audio(&project));
    rig.engine.play();
    let got = rig.pull(2_500);
    assert_eq!(got.len(), ms(2_500));
    // Media 1, past its 10 ms fades: the tone at full level.
    for (a, b) in [(10, 990), (2_010, 2_490)] {
        let level = tone_level(&got[ms(a)..ms(b)], 1_000.0, AUDIO_RATE);
        assert!(level > 0.9 * TONE, "[{a}, {b}) ms: {level}");
    }
    // Media 2 (clip audio off) and no music: nothing at all.
    assert!(got[ms(1_000)..ms(2_000)].iter().all(|s| *s == 0.0));
    // The same samples export mixes.
    assert_eq!(got, rig.bed.samples);

    // Clip audio off: silence everywhere (there is no music).
    project.media[0].clip_audio = false;
    rig.engine
        .set_edit(&project, Project::DEFAULT_SECTION)
        .unwrap();
    assert!(rig.engine.wait_for_audio(WAIT));
    rig.engine.seek(Micros(0));
    rig.engine.play();
    assert!(rig.pull(2_500).iter().all(|s| *s == 0.0));
}

#[test]
fn clip_audio_swaps_in_while_playing_and_volume_scales_only_the_music() {
    let music = kv_testkit::click_track(120.0, 3.0, AUDIO_RATE);
    let mut project = tone_project(false);
    project.sections[0].music = Some(MusicRef {
        path: music.to_str().unwrap().to_string(),
        duration: Micros(3_000_000),
    });
    project.settings.music_volume_percent = 50;
    let mut opts = MusicBedOptions::new(AUDIO_RATE, 1, TONE_LEN);
    opts.fade_out = crate::MUSIC_FADE_OUT;
    let bed = render_music_bed(&music, &opts).unwrap();
    let clip = tone_clip_audio(&project);
    let rig = AudioRig::build(&project, bed);
    let close = |got: &[f32], want: &dyn Fn(usize) -> f32, from: usize| {
        for (i, g) in got.iter().enumerate() {
            let w = want(from + i);
            assert!((g - w).abs() < 1e-6, "frame {}: {g} vs {w}", from + i);
        }
    };

    // Music only, at half volume (live transport gain).
    rig.engine.play();
    let got = rig.pull(300);
    close(&got, &|k| rig.bed.samples[k] * 0.5, 0);

    // Clip audio on while playing: the mix replaces the bed with no jump
    // in the clock or the bed position.
    project.media[0].clip_audio = true;
    rig.engine
        .set_edit(&project, Project::DEFAULT_SECTION)
        .unwrap();
    assert!(rig.engine.wait_for_audio(WAIT));
    assert!(rig.engine.is_playing());
    assert_eq!(rig.engine.position(), Micros(300_000));
    let got = rig.pull(400);
    assert_eq!(rig.engine.position(), Micros(700_000));
    close(
        &got,
        &|k| rig.bed.samples[k] * 0.5 + clip.samples[k],
        ms(300),
    );

    // Volume to 100 %: only the music part gets louder.
    project.settings.music_volume_percent = 100;
    rig.engine
        .set_edit(&project, Project::DEFAULT_SECTION)
        .unwrap();
    assert!(rig.engine.wait_for_audio(WAIT));
    let got = rig.pull(300);
    close(&got, &|k| rig.bed.samples[k] + clip.samples[k], ms(700));
}
