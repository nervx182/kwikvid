use super::*;

use std::path::Path;

use kv_core::{
    AspectRatio, Edl, MediaId, MediaItem, MediaKind, ProjectId, Segment, Speed, Transition,
};
use kv_media::{DecodeOptions, MediaError, MediaInfo, VideoReader};
use kv_render::preview::{CpuCompositor, ManualClock, Tick};
use tracing_subscriber::layer::SubscriberExt;

const SEC: i64 = 1_000_000;

fn project(len_secs: i64) -> Project {
    let mut p = Project::new(ProjectId(1));
    p.media = vec![MediaItem {
        id: MediaId(1),
        path: "clips/clip1.mp4".into(),
        duration: Micros(60 * SEC),
        kind: MediaKind::Video,
        hilights: vec![],
        clip_audio: false,
        chapters: Vec::new(),
    }];
    let mut edl = Edl {
        segments: vec![Segment {
            media_id: MediaId(1),
            src_in: Micros(0),
            src_out: Micros(len_secs * SEC),
            timeline_start: Micros(0),
            transition: Transition::Cut,
            speed: Speed::NORMAL,
        }],
    };
    edl.repack();
    p.sections[0].edit = edl;
    p
}

/// Every call a fake engine received.
#[derive(Debug, Clone, PartialEq)]
enum Call {
    SetEdit(Micros),
    Play,
    Pause,
    Seek(Micros),
}

struct FakeState {
    calls: Vec<Call>,
    position: Micros,
    playing: bool,
    end: Micros,
}

impl Default for FakeState {
    fn default() -> Self {
        FakeState {
            calls: Vec::new(),
            position: Micros(0),
            playing: false,
            end: Micros(0),
        }
    }
}

/// A fake engine: keeps position and play state like the real one (an
/// edit keeps both), and logs calls.
#[derive(Default, Clone)]
struct Fake(Arc<Mutex<FakeState>>);

impl Fake {
    fn calls(&self) -> Vec<Call> {
        lock(&self.0).calls.clone()
    }
}

fn edit_end(project: &Project) -> Micros {
    let s = &project.sections[0].edit.segments;
    Micros(
        s.iter()
            .map(|s| s.timeline_start.0 + (s.src_out.0 - s.src_in.0))
            .max()
            .unwrap_or(0),
    )
}

impl PreviewControl for Fake {
    fn set_edit(&self, project: &Project, _: SectionId) -> Result<(), String> {
        let mut s = lock(&self.0);
        s.end = edit_end(project);
        s.position = Micros(s.position.0.min(s.end.0));
        let end = s.end;
        s.calls.push(Call::SetEdit(end));
        Ok(())
    }
    fn play(&self) {
        let mut s = lock(&self.0);
        s.playing = true;
        s.calls.push(Call::Play);
    }
    fn pause(&self) {
        let mut s = lock(&self.0);
        s.playing = false;
        s.calls.push(Call::Pause);
    }
    fn seek(&self, t: Micros) {
        let mut s = lock(&self.0);
        s.position = Micros(t.0.clamp(0, s.end.0));
        s.calls.push(Call::Seek(t));
    }
    fn position(&self) -> Micros {
        lock(&self.0).position
    }
    fn is_playing(&self) -> bool {
        lock(&self.0).playing
    }
    fn stats(&self) -> PreviewStats {
        PreviewStats {
            presented: 7,
            ..PreviewStats::default()
        }
    }
}

/// Every engine a fake factory built, with its spec.
type Built = Arc<Mutex<Vec<(EngineSpec, Fake)>>>;

/// A transport whose factory hands out fakes, recording each spec.
fn fake_transport() -> (Transport, Built) {
    let built: Built = Arc::default();
    let t = Transport::new(Arc::default());
    let b = Arc::clone(&built);
    t.set_factory(Box::new(move |spec| {
        let fake = Fake::default();
        lock(&b).push((spec.clone(), fake.clone()));
        Ok(Arc::new(fake) as Arc<dyn PreviewControl>)
    }));
    (t, built)
}

fn root() -> PathBuf {
    PathBuf::from("/home/ana")
}

#[test]
fn without_an_engine_the_transport_is_stopped_and_seek_echoes() {
    let t = Transport::new(Arc::default());
    assert!(!t.play().playing);
    assert_eq!(t.seek(Micros(1_500_000)).position, Micros(1_500_000));
    assert_eq!(t.seek(Micros(-5)).position, Micros(0));
    assert_eq!(t.pause().position, Micros(0));
    // A project without a factory builds nothing.
    t.request_sync(project(4), root());
    t.sync();
    assert!(!t.has_engine());
}

#[test]
fn commands_drive_the_engine() {
    let (t, built) = fake_transport();
    t.request_sync(project(4), root());
    t.sync();
    let fake = lock(&built)[0].1.clone();
    assert_eq!(fake.calls(), vec![Call::SetEdit(Micros(4 * SEC))]);

    let s = t.play();
    assert!(s.playing);
    assert!((s.frame_us - 33_333.333).abs() < 0.01);
    let s = t.seek(Micros(2 * SEC));
    assert_eq!((s.position, s.playing), (Micros(2 * SEC), true));
    // The engine clamps to the end; the answer says where it went.
    assert_eq!(t.seek(Micros(9 * SEC)).position, Micros(4 * SEC));
    let s = t.pause();
    assert!(!s.playing);
    assert_eq!(t.status(), s);
    assert_eq!(
        fake.calls()[1..],
        [
            Call::Play,
            Call::Seek(Micros(2 * SEC)),
            Call::Seek(Micros(9 * SEC)),
            Call::Pause
        ]
    );
    assert_eq!(t.debug("off").presented, 7);
    assert!(t.debug("off").engine);
}

#[test]
fn editing_while_playing_keeps_playing_from_the_same_position() {
    let (t, built) = fake_transport();
    t.request_sync(project(4), root());
    t.sync();
    t.play();
    t.seek(Micros(SEC));
    // The same project again (a save, a page reload): nothing to do.
    t.request_sync(project(4), root());
    t.sync();
    // A real edit: set_edit on the same engine, no seek, no pause.
    t.request_sync(project(6), root());
    t.sync();
    assert_eq!(lock(&built).len(), 1, "no new engine for an edit");
    let fake = lock(&built)[0].1.clone();
    assert_eq!(
        fake.calls(),
        vec![
            Call::SetEdit(Micros(4 * SEC)),
            Call::Play,
            Call::Seek(Micros(SEC)),
            Call::SetEdit(Micros(6 * SEC)),
        ]
    );
    let s = t.status();
    assert_eq!((s.position, s.playing), (Micros(SEC), true));
}

#[test]
fn aspect_change_rebuilds_at_the_same_position_and_a_new_root_starts_over() {
    let (t, built) = fake_transport();
    t.set_viewer((3840, 2160));
    t.request_sync(project(4), root());
    t.sync();
    assert_eq!(
        lock(&built)[0].0.size,
        (1920, 1080),
        "big viewer: full size"
    );
    t.seek(Micros(3 * SEC));

    let mut square = project(4);
    square.settings.aspect_ratio = AspectRatio::Square1x1;
    t.set_viewer((0, 0));
    t.request_sync(square.clone(), root());
    t.sync();
    {
        let b = lock(&built);
        assert_eq!(b.len(), 2);
        assert_eq!(b[1].0.aspect, AspectRatio::Square1x1);
        assert_eq!(b[1].0.size, (540, 540), "proxy size");
        assert_eq!(
            b[1].1.calls(),
            vec![Call::SetEdit(Micros(4 * SEC)), Call::Seek(Micros(3 * SEC))]
        );
    }
    assert_eq!(t.status().position, Micros(3 * SEC));

    t.request_sync(square, PathBuf::from("/projects/trip"));
    t.sync();
    let b = lock(&built);
    assert_eq!(b.len(), 3);
    assert_eq!(b[2].0.media_root, PathBuf::from("/projects/trip"));
    assert_eq!(b[2].1.calls(), vec![Call::SetEdit(Micros(4 * SEC))]);
}

#[test]
fn a_failing_factory_leaves_the_transport_stopped() {
    let t = Transport::new(Arc::default());
    t.set_factory(Box::new(|_| Err("no GPU".into())));
    t.request_sync(project(4), root());
    t.sync();
    assert!(!t.has_engine());
    assert_eq!(t.seek(Micros(SEC)).position, Micros(SEC));
}

#[test]
fn position_events_only_on_change() {
    let mut w = PositionWatch::default();
    let s = |p: i64, playing: bool| TransportStatus {
        position: Micros(p),
        playing,
        frame_us: 33_333.0,
    };
    assert_eq!(w.update(s(0, false)), Some(s(0, false)));
    assert_eq!(w.update(s(0, false)), None);
    assert_eq!(w.update(s(0, true)), Some(s(0, true)));
    assert_eq!(w.update(s(66_000, true)), Some(s(66_000, true)));
    assert_eq!(w.update(s(66_000, true)), None);
    // At most one event per interval: ~15 Hz.
    assert!(POSITION_INTERVAL >= Duration::from_millis(60));
    assert!(POSITION_INTERVAL <= Duration::from_millis(70));
}

/// What the UI receives (`ui/js/transport.js`, the mock in `ui-tests`).
#[test]
fn status_and_stats_json() {
    let s = TransportStatus {
        position: Micros(1_500_000),
        playing: true,
        frame_us: 33_333.5,
    };
    assert_eq!(
        serde_json::to_value(s).unwrap(),
        serde_json::json!({ "position": 1_500_000, "playing": true, "frame_us": 33_333.5 })
    );
    let t = Transport::new(Arc::default());
    let d = serde_json::to_value(t.debug("off")).unwrap();
    assert_eq!(
        d,
        serde_json::json!({
            "backend": "off", "engine": false, "presented": 0, "dropped": 0,
            "seek": { "count": 0, "last_ms": null, "p50_ms": null, "p95_ms": null }
        })
    );
}

#[test]
fn percentiles_are_nearest_rank() {
    let l = SeekLatencies::default();
    for us in (1..=100).rev() {
        l.push(us * 1000);
    }
    let s = l.summary();
    assert_eq!(s.count, 100);
    assert_eq!(s.last_ms, Some(1.0));
    assert_eq!(s.p50_ms, Some(50.0));
    assert_eq!(s.p95_ms, Some(95.0));
    assert_eq!(percentile(&[], 0.5), None);
    assert_eq!(percentile(&[7], 0.95), Some(7));
    // Only the newest samples count for the percentiles.
    for _ in 0..KEEP_LATENCIES {
        l.push(2000);
    }
    let s = l.summary();
    assert_eq!(s.count, 100 + KEEP_LATENCIES as u64);
    assert_eq!((s.p50_ms, s.p95_ms), (Some(2.0), Some(2.0)));
}

#[test]
fn layer_collects_latency_from_the_seek_span_only() {
    let lat = Arc::new(SeekLatencies::default());
    let subscriber = tracing_subscriber::registry().with(seek_latency_layer(Arc::clone(&lat)));
    tracing::subscriber::with_default(subscriber, || {
        let span = tracing::info_span!(
            "preview_seek",
            target_us = 5,
            latency_us = tracing::field::Empty
        );
        span.record("latency_us", 42_000_u64);
        let other = tracing::info_span!("other", latency_us = tracing::field::Empty);
        other.record("latency_us", 1_u64);
    });
    let s = lat.summary();
    assert_eq!(s.count, 1);
    assert_eq!(s.last_ms, Some(42.0));
}

/// Media that never opens: the engine shows black frames, no decoding.
struct NoMedia;

impl MediaBackend for NoMedia {
    fn probe(&self, path: &Path) -> kv_media::Result<MediaInfo> {
        Err(MediaError::NoVideoStream(path.to_path_buf()))
    }
    fn open_video(&self, path: &Path, _: &DecodeOptions) -> kv_media::Result<Box<dyn VideoReader>> {
        Err(MediaError::NoVideoStream(path.to_path_buf()))
    }
}

/// The real engine on a manual clock, ticked by the test (no pacer).
struct Manual(PreviewEngine);

impl PreviewControl for Manual {
    fn set_edit(&self, project: &Project, section: SectionId) -> Result<(), String> {
        self.0.set_edit(project, section).map_err(|e| e.to_string())
    }
    fn play(&self) {
        self.0.play();
    }
    fn pause(&self) {
        self.0.pause();
    }
    fn seek(&self, t: Micros) {
        self.0.seek(t);
    }
    fn position(&self) -> Micros {
        self.0.position()
    }
    fn is_playing(&self) -> bool {
        self.0.is_playing()
    }
    fn stats(&self) -> PreviewStats {
        self.0.stats()
    }
}

/// End to end with the real engine: seek latency reaches the overlay
/// through the span, and an edit while playing keeps the position.
#[test]
fn real_engine_measures_seeks_and_keeps_playing_through_an_edit() {
    let clock = Arc::new(ManualClock::new());
    let engine = Arc::new(Manual(
        PreviewEngine::new(
            Arc::new(NoMedia),
            Box::new(CpuCompositor::new(32, 18).unwrap()),
            Box::new(NullSink),
            PreviewConfig {
                fps: PREVIEW_FPS,
                clock: clock.clone(),
                decode_threads: 1,
                ..PreviewConfig::default()
            },
        )
        .unwrap(),
    ));
    let lat = Arc::new(SeekLatencies::default());
    let t = Transport::new(Arc::clone(&lat));
    let e = Arc::clone(&engine);
    t.set_factory(Box::new(move |_| {
        Ok(Arc::clone(&e) as Arc<dyn PreviewControl>)
    }));
    let subscriber = tracing_subscriber::registry().with(seek_latency_layer(Arc::clone(&lat)));
    tracing::subscriber::with_default(subscriber, || {
        t.request_sync(project(4), root());
        t.sync();
        t.seek(Micros(SEC));
        assert!(matches!(engine.0.tick().unwrap(), Tick::Presented(30)));
    });
    assert_eq!(t.debug("off").seek.count, 1);
    assert!(t.debug("off").seek.p95_ms.is_some());

    assert!(t.play().playing);
    clock.advance(Micros(SEC / 2));
    t.request_sync(project(6), root());
    t.sync();
    let s = t.status();
    assert!(s.playing, "still playing after the edit");
    assert_eq!(s.position, Micros(SEC + SEC / 2));
    clock.advance(Micros(SEC / 2));
    assert_eq!(t.status().position, Micros(2 * SEC));
}

#[test]
fn a_viewer_resize_rebuilds_at_the_new_size_keeping_position_and_play_state() {
    let (t, built) = fake_transport();
    let t0 = Instant::now();
    t.set_viewer_at((960, 540), t0);
    t.request_sync(project(4), root());
    t.sync_at(t0);
    assert_eq!(lock(&built)[0].0.size, (960, 540));
    t.seek(Micros(2 * SEC));
    t.play();

    // Enlarging the window (or moving it to a Retina display).
    t.set_viewer_at((3840, 2160), t0);
    // Before the debounce: still the old engine, even when an edit syncs.
    t.request_sync(project(6), root());
    t.sync_at(t0 + RESIZE_DEBOUNCE / 2);
    assert_eq!(lock(&built).len(), 1, "no rebuild while debouncing");
    t.sync_at(t0 + RESIZE_DEBOUNCE);
    {
        let b = lock(&built);
        assert_eq!(b.len(), 2);
        assert_eq!(b[1].0.size, (1920, 1080));
        assert_eq!(
            b[1].1.calls(),
            vec![
                Call::SetEdit(Micros(6 * SEC)),
                Call::Seek(Micros(2 * SEC)),
                Call::Play
            ]
        );
    }
    let s = t.status();
    assert_eq!((s.position, s.playing), (Micros(2 * SEC), true));
    // Nothing more to do once rebuilt.
    t.sync_at(t0 + RESIZE_DEBOUNCE * 4);
    assert_eq!(lock(&built).len(), 2);
    assert_eq!(t.resize_wait(t0), None);
}

#[test]
fn viewer_changes_that_keep_the_preview_size_do_nothing() {
    let (t, built) = fake_transport();
    let t0 = Instant::now();
    t.request_sync(project(4), root());
    t.sync_at(t0);
    assert_eq!(
        lock(&built)[0].0.size,
        (960, 540),
        "proxy before the viewer"
    );
    // Smaller than the output: still the proxy size.
    for size in [(800, 450), (1200, 700), (1919, 1079)] {
        t.set_viewer_at(size, t0);
        assert_eq!(t.resize_wait(t0), None, "{size:?}");
    }
    t.sync_at(t0 + RESIZE_DEBOUNCE * 2);
    assert_eq!(lock(&built).len(), 1);
    // A drag that ends where it started cancels the pending resize.
    t.set_viewer_at((3840, 2160), t0);
    assert_eq!(t.resize_wait(t0), Some(RESIZE_DEBOUNCE));
    t.set_viewer_at((960, 540), t0);
    assert_eq!(t.resize_wait(t0), None);
    t.sync_at(t0 + RESIZE_DEBOUNCE * 2);
    assert_eq!(lock(&built).len(), 1);
    assert_eq!(
        lock(&built)[0].1.calls(),
        vec![Call::SetEdit(Micros(4 * SEC))]
    );
}

#[test]
fn resizes_are_debounced_from_the_last_one() {
    let (t, built) = fake_transport();
    let t0 = Instant::now();
    t.request_sync(project(4), root());
    t.sync_at(t0);
    // A drag: one report per frame for a while.
    let mut now = t0;
    for i in 0..30u32 {
        now = t0 + Duration::from_millis(16) * i;
        t.set_viewer_at((1920 + 4 * i, 1080 + 4 * i), now);
        t.sync_at(now);
    }
    assert_eq!(lock(&built).len(), 1, "no rebuild during the drag");
    assert_eq!(t.resize_wait(now), Some(RESIZE_DEBOUNCE));
    t.sync_at(now + RESIZE_DEBOUNCE - Duration::from_millis(1));
    assert_eq!(lock(&built).len(), 1);
    t.sync_at(now + RESIZE_DEBOUNCE);
    assert_eq!(lock(&built).len(), 2, "one rebuild after it");
    assert_eq!(lock(&built)[1].0.size, (1920, 1080));
}

#[test]
fn the_worker_rebuilds_once_after_the_resizes_stop() {
    let (t, built) = fake_transport();
    let t = Arc::new(t);
    t.spawn_worker();
    t.request_sync(project(4), root());
    let wait_for = |n: usize| {
        let deadline = Instant::now() + Duration::from_secs(5);
        while lock(&built).len() < n && Instant::now() < deadline {
            std::thread::yield_now();
        }
        lock(&built).len()
    };
    assert_eq!(wait_for(1), 1);
    let start = Instant::now();
    t.set_viewer((3000, 1700));
    std::thread::sleep(RESIZE_DEBOUNCE / 5);
    t.set_viewer((3840, 2160));
    let last = Instant::now();
    assert_eq!(wait_for(2), 2);
    assert!(
        last.elapsed() >= RESIZE_DEBOUNCE,
        "debounced from the last resize"
    );
    assert!(start.elapsed() < Duration::from_secs(5));
    assert_eq!(lock(&built)[1].0.size, (1920, 1080));
    std::thread::sleep(RESIZE_DEBOUNCE);
    assert_eq!(lock(&built).len(), 2);
}
