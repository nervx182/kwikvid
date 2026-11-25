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
        engine.set_edit(&project, Project::DEFAULT_SECTION).unwrap();

        let mut opts = MusicBedOptions::new(AUDIO_RATE, 1, Micros(2_000_000));
        opts.fade_out = crate::MUSIC_FADE_OUT;
        let bed = render_music_bed(&music, &opts).unwrap();
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
