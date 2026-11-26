//! Transport behind `ui/js/transport.js`: play, pause, seek and the playhead
//! position, over the preview engine ([`PreviewEngine`], kv-render).
//!
//! - **Commands** `play`, `pause`, `seek { position }` and `get_transport`
//!   answer a [`TransportStatus`]. They are synchronous Tauri commands, so
//!   they run in call order; each only takes the engine's lock briefly.
//! - **Position**: `kv://position` ([`POSITION_EVENT`]) carries a
//!   [`TransportStatus`] at most every [`POSITION_INTERVAL`] (about 15 Hz)
//!   and only when it changed. A watcher thread polls the engine
//!   ([`PositionWatch`]) instead of using its per-frame callback, so the
//!   rate is bounded and the end of playback (where no frame is presented)
//!   is reported too.
//! - **Edits**: every new state is handed to [`Transport::request_sync`]; a
//!   worker thread calls the engine's `set_edit`, which keeps the position
//!   and the play state (editing while playing keeps playing). It runs off
//!   the main thread because a new song renders its music bed first. A new
//!   aspect ratio or media root (another project) builds a new engine.
//! - **Viewer size**: `set_viewer_rect` reports the viewer's physical size
//!   ([`Transport::set_viewer`]). When that changes the preview size
//!   ([`frame_size`]), the engine is rebuilt at the new size, keeping the
//!   position and play state, [`RESIZE_DEBOUNCE`] after the last such
//!   report, so a window drag rebuilds once.
//! - **Seek latency**: the engine's `preview_seek` span records
//!   `latency_us` (from `seek`, called as the command is received, to the
//!   target frame being presented). [`seek_latency_layer`] collects it into
//!   [`SeekLatencies`]; p50/p95 are logged every [`LOG_EVERY`] seeks and
//!   served by `get_preview_stats` to the debug overlay.
//! - Until an engine exists (macOS: until the native layer is installed;
//!   or when FFmpeg is unavailable) the commands answer as a stopped
//!   transport and `seek` echoes the target.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use kv_core::{AspectRatio, Micros, Project, SectionId};
use kv_media::{MediaBackend, Rational};
use kv_render::preview::{
    preview_size, AudioTransport, FrameSink, Pacer, PreviewCompositor, PreviewConfig,
    PreviewEngine, PreviewStats,
};
use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager, Runtime, State};
use tracing::field::{Field, Visit};
use tracing::span;
use tracing_subscriber::filter::filter_fn;
use tracing_subscriber::layer::Context;
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::Layer;

use crate::ipc::AppState;
use crate::preview::{test_pattern_size, PreviewState};

/// The event carrying the playhead while it moves.
pub const POSITION_EVENT: &str = "kv://position";
/// Shortest gap between two `kv://position` events (~15 Hz, guess).
pub const POSITION_INTERVAL: Duration = Duration::from_millis(66);
/// Preview frame rate (the engine's default; the project has no rate yet).
pub const PREVIEW_FPS: Rational = Rational::new(30, 1);
/// The engine's seek span (`kv_render::preview`, field `latency_us`).
pub const SEEK_SPAN: &str = "preview_seek";
/// Seek latencies kept for the percentiles.
const KEEP_LATENCIES: usize = 500;
/// Log p50/p95 after this many seeks.
pub const LOG_EVERY: u64 = 20;
/// Quiet time after the last viewer resize before the engine is rebuilt
/// at the new preview size (guess).
pub const RESIZE_DEBOUNCE: Duration = Duration::from_millis(250);
/// The MVP has one section.
const SECTION: SectionId = Project::DEFAULT_SECTION;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    // Nothing here has invariants a panic could break half-way.
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// What the transport commands and `kv://position` carry.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct TransportStatus {
    /// Playhead on the section timeline.
    pub position: Micros,
    pub playing: bool,
    /// Length of one preview frame in microseconds (frame stepping and the
    /// m:ss.ff readout).
    pub frame_us: f64,
}

/// What the transport needs from a preview engine; [`PacedEngine`] in the
/// app, a fake in tests.
pub trait PreviewControl: Send + Sync {
    fn set_edit(&self, project: &Project, section: SectionId) -> Result<(), String>;
    fn play(&self);
    fn pause(&self);
    fn seek(&self, t: Micros);
    fn position(&self) -> Micros;
    fn is_playing(&self) -> bool;
    fn stats(&self) -> PreviewStats;
}

/// A [`PreviewEngine`] with a [`Pacer`] ticking it; dropping it stops both.
pub struct PacedEngine {
    engine: Arc<PreviewEngine>,
    _pacer: Pacer,
}

impl PacedEngine {
    pub fn new(engine: PreviewEngine) -> PacedEngine {
        let engine = Arc::new(engine);
        let pacer = Pacer::spawn(Arc::clone(&engine));
        PacedEngine {
            engine,
            _pacer: pacer,
        }
    }
}

impl PreviewControl for PacedEngine {
    fn set_edit(&self, project: &Project, section: SectionId) -> Result<(), String> {
        self.engine
            .set_edit(project, section)
            .map_err(|e| e.to_string())
    }
    fn play(&self) {
        self.engine.play();
    }
    fn pause(&self) {
        self.engine.pause();
    }
    fn seek(&self, t: Micros) {
        self.engine.seek(t);
    }
    fn position(&self) -> Micros {
        self.engine.position()
    }
    fn is_playing(&self) -> bool {
        self.engine.is_playing()
    }
    fn stats(&self) -> PreviewStats {
        self.engine.stats()
    }
}

/// What an engine is built for. A change of `aspect` or `media_root`
/// needs a new engine (compositor size, path resolution); `size` is the
/// preview size chosen at build time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EngineSpec {
    pub aspect: AspectRatio,
    pub media_root: PathBuf,
    pub size: (u32, u32),
}

/// Builds an engine for a spec.
pub type EngineFactory =
    Box<dyn Fn(&EngineSpec) -> Result<Arc<dyn PreviewControl>, String> + Send + Sync>;

/// The compositor and sink for a preview size.
pub type Outputs = (Box<dyn PreviewCompositor>, Box<dyn FrameSink>);

/// A factory for paced [`PreviewEngine`]s decoding with `media`, playing
/// `audio` (if any) and drawing through `outputs(size)`.
pub fn engine_factory(
    media: Arc<dyn MediaBackend>,
    audio: Option<Arc<AudioTransport>>,
    outputs: impl Fn((u32, u32)) -> Result<Outputs, String> + Send + Sync + 'static,
) -> EngineFactory {
    Box::new(move |spec| {
        let (compositor, sink) = outputs(spec.size)?;
        let config = PreviewConfig {
            fps: PREVIEW_FPS,
            media_root: spec.media_root.clone(),
            audio: audio.clone(),
            ..PreviewConfig::default()
        };
        let engine = PreviewEngine::new(Arc::clone(&media), compositor, sink, config)
            .map_err(|e| e.to_string())?;
        Ok(Arc::new(PacedEngine::new(engine)) as Arc<dyn PreviewControl>)
    })
}

/// A sink that shows nothing, for runs without a preview surface
/// (`KV_PREVIEW=off`): the transport still works.
pub struct NullSink;

impl FrameSink for NullSink {
    fn present(&mut self, _: &kv_render::preview::RenderedFrame<'_>) {}
}

/// The preview size for a project aspect ratio in a viewer of `viewer`
/// physical pixels (`(0, 0)` before the viewer is known: the proxy size).
pub fn frame_size(aspect: AspectRatio, viewer: (u32, u32)) -> (u32, u32) {
    preview_size(test_pattern_size(aspect), viewer)
}

struct Attached {
    spec: EngineSpec,
    engine: Arc<dyn PreviewControl>,
    /// The project last given to `set_edit`.
    edit: Option<Project>,
}

#[derive(Default)]
struct Inner {
    factory: Option<EngineFactory>,
    attached: Option<Attached>,
    /// The newest project and media root (synced by [`Transport::sync`]).
    latest: Option<(Project, PathBuf)>,
    viewer: (u32, u32),
    /// When the engine is due to be rebuilt for a new viewer size (set by
    /// [`Transport::set_viewer`], pushed back by every further resize).
    resize_due: Option<Instant>,
}

/// Tauri managed state (as `Arc<Transport>`): the engine and the seek
/// latency samples.
///
/// Lock order: `dirty` before `inner`, never the other way round.
pub struct Transport {
    inner: Mutex<Inner>,
    dirty: Mutex<bool>,
    wake: Condvar,
    latencies: Arc<SeekLatencies>,
}

impl Transport {
    pub fn new(latencies: Arc<SeekLatencies>) -> Transport {
        Transport {
            inner: Mutex::new(Inner::default()),
            dirty: Mutex::new(false),
            wake: Condvar::new(),
            latencies,
        }
    }

    fn frame_us() -> f64 {
        1_000_000.0 / PREVIEW_FPS.as_f64()
    }

    fn engine(&self) -> Option<Arc<dyn PreviewControl>> {
        lock(&self.inner)
            .attached
            .as_ref()
            .map(|a| Arc::clone(&a.engine))
    }

    /// True once an engine is built.
    pub fn has_engine(&self) -> bool {
        lock(&self.inner).attached.is_some()
    }

    /// True once engines can be built.
    // The native layer (macOS) installs its factory late.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub fn has_factory(&self) -> bool {
        lock(&self.inner).factory.is_some()
    }

    /// Where engines come from (once the preview surface exists). The next
    /// sync builds one for the latest project.
    pub fn set_factory(&self, factory: EngineFactory) {
        lock(&self.inner).factory = Some(factory);
        self.mark_dirty();
    }

    /// The viewer's physical size. When it changes the running engine's
    /// preview size, the worker rebuilds the engine [`RESIZE_DEBOUNCE`]
    /// after the last such call.
    pub fn set_viewer(&self, size: (u32, u32)) {
        self.set_viewer_at(size, Instant::now());
    }

    fn set_viewer_at(&self, size: (u32, u32), now: Instant) {
        {
            let mut g = lock(&self.inner);
            g.viewer = size;
            let resize = g
                .attached
                .as_ref()
                .is_some_and(|a| frame_size(a.spec.aspect, size) != a.spec.size);
            // Back to the engine's size (mid-drag): nothing to do.
            g.resize_due = resize.then(|| now + RESIZE_DEBOUNCE);
        }
        // Let the worker pick up the new deadline.
        let _dirty = lock(&self.dirty);
        self.wake.notify_all();
    }

    /// Time left until a pending resize is due (zero when overdue), `None`
    /// when there is none.
    fn resize_wait(&self, now: Instant) -> Option<Duration> {
        lock(&self.inner)
            .resize_due
            .map(|due| due.saturating_duration_since(now))
    }

    /// The project changed: sync the engine on the worker thread.
    pub fn request_sync(&self, project: Project, media_root: PathBuf) {
        lock(&self.inner).latest = Some((project, media_root));
        self.mark_dirty();
    }

    fn mark_dirty(&self) {
        *lock(&self.dirty) = true;
        self.wake.notify_all();
    }

    /// Bring the engine in line with the latest project: build one when
    /// there is none, the aspect ratio or media root changed, or a viewer
    /// resize is due that changes the preview size (keeping the position
    /// and play state unless the media root changed), else `set_edit` when
    /// the project differs from the last one shown. Blocks while the music
    /// bed renders; the worker thread calls it.
    pub fn sync(&self) {
        self.sync_at(Instant::now());
    }

    fn sync_at(&self, now: Instant) {
        let mut g = lock(&self.inner);
        if g.resize_due.is_some_and(|due| now >= due) {
            g.resize_due = None;
        }
        let Some((project, root)) = g.latest.clone() else {
            return;
        };
        let aspect = project.settings.aspect_ratio;
        let size = frame_size(aspect, g.viewer);
        // While the viewer is still being resized, keep the current size.
        let size_ok = |a: &Attached| a.spec.size == size || g.resize_due.is_some();
        let (engine, old, carry) = match &g.attached {
            Some(a) if a.spec.aspect == aspect && a.spec.media_root == root && size_ok(a) => {
                if a.edit.as_ref() == Some(&project) {
                    return;
                }
                (Arc::clone(&a.engine), None, None)
            }
            _ => {
                let Some(factory) = &g.factory else { return };
                let spec = EngineSpec {
                    aspect,
                    media_root: root,
                    size,
                };
                let engine = match factory(&spec) {
                    Ok(e) => e,
                    Err(e) => {
                        tracing::warn!("preview engine unavailable: {e}");
                        return;
                    }
                };
                tracing::info!(size = ?spec.size, "preview engine started");
                // Read last, so the old engine's playhead moved as far as
                // it will before the new one takes over.
                let carry = g
                    .attached
                    .as_ref()
                    .filter(|a| a.spec.media_root == spec.media_root)
                    .map(|a| (a.engine.position(), a.engine.is_playing()));
                g.resize_due = None;
                let old = g.attached.replace(Attached {
                    spec,
                    engine: Arc::clone(&engine),
                    edit: None,
                });
                (engine, old, carry)
            }
        };
        drop(g);
        // The old engine's threads are joined outside the lock.
        drop(old);
        if let Err(e) = engine.set_edit(&project, SECTION) {
            tracing::warn!("preview: cannot show the edit: {e}");
        }
        if let Some((pos, playing)) = carry {
            engine.seek(pos);
            if playing {
                engine.play();
            }
        }
        if let Some(a) = &mut lock(&self.inner).attached {
            if Arc::ptr_eq(&a.engine, &engine) {
                a.edit = Some(project);
            }
        }
    }

    /// Runs [`sync`](Self::sync) on its own thread whenever something
    /// changed or a viewer resize is due, coalescing bursts of edits. For
    /// the app's lifetime.
    pub fn spawn_worker(self: &Arc<Self>) {
        let t = Arc::clone(self);
        let spawned = std::thread::Builder::new()
            .name("kv-transport-sync".into())
            .spawn(move || loop {
                {
                    let mut dirty = lock(&t.dirty);
                    while !*dirty {
                        dirty = match t.resize_wait(Instant::now()) {
                            Some(left) if left.is_zero() => break,
                            Some(left) => {
                                t.wake
                                    .wait_timeout(dirty, left)
                                    .unwrap_or_else(PoisonError::into_inner)
                                    .0
                            }
                            None => t.wake.wait(dirty).unwrap_or_else(PoisonError::into_inner),
                        };
                    }
                    *dirty = false;
                }
                t.sync();
            });
        if let Err(e) = spawned {
            tracing::error!("cannot start the transport worker: {e}");
        }
    }

    /// Position and play state.
    pub fn status(&self) -> TransportStatus {
        let (position, playing) = self
            .engine()
            .map_or((Micros(0), false), |e| (e.position(), e.is_playing()));
        TransportStatus {
            position,
            playing,
            frame_us: Self::frame_us(),
        }
    }

    pub fn play(&self) -> TransportStatus {
        if let Some(e) = self.engine() {
            e.play();
        }
        self.status()
    }

    pub fn pause(&self) -> TransportStatus {
        if let Some(e) = self.engine() {
            e.pause();
        }
        self.status()
    }

    /// Jump to `position` (never before the start; the engine also clamps
    /// to the edit's end), keeping the play state.
    pub fn seek(&self, position: Micros) -> TransportStatus {
        let target = Micros(position.0.max(0));
        match self.engine() {
            Some(e) => {
                e.seek(target);
                self.status()
            }
            None => TransportStatus {
                position: target,
                playing: false,
                frame_us: Self::frame_us(),
            },
        }
    }

    /// Counters for the debug overlay.
    pub fn debug(&self, backend: &'static str) -> PreviewDebug {
        let stats = self.engine().map(|e| e.stats()).unwrap_or_default();
        PreviewDebug {
            backend,
            engine: self.has_engine(),
            presented: stats.presented,
            dropped: stats.dropped,
            seek: self.latencies.summary(),
        }
    }

    /// Emits `kv://position` on a thread, at most every
    /// [`POSITION_INTERVAL`] and only when the status changed. For the
    /// app's lifetime.
    pub fn spawn_position_events<R: Runtime>(self: &Arc<Self>, app: AppHandle<R>) {
        let t = Arc::clone(self);
        let spawned = std::thread::Builder::new()
            .name("kv-transport-position".into())
            .spawn(move || {
                let mut watch = PositionWatch::default();
                loop {
                    std::thread::sleep(POSITION_INTERVAL);
                    if !t.has_engine() {
                        continue;
                    }
                    if let Some(s) = watch.update(t.status()) {
                        if let Err(e) = app.emit(POSITION_EVENT, s) {
                            tracing::warn!("could not emit {POSITION_EVENT}: {e}");
                        }
                    }
                }
            });
        if let Err(e) = spawned {
            tracing::error!("cannot start the position events: {e}");
        }
    }
}

/// Decides which polled statuses become `kv://position` events: only
/// changes.
#[derive(Debug, Default)]
pub struct PositionWatch {
    last: Option<TransportStatus>,
}

impl PositionWatch {
    /// `Some(status)` when it differs from the last one reported.
    pub fn update(&mut self, status: TransportStatus) -> Option<TransportStatus> {
        if self.last == Some(status) {
            return None;
        }
        self.last = Some(status);
        Some(status)
    }
}

/// Seek latency percentiles for the overlay, in milliseconds.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct SeekSummary {
    /// Seeks measured since the app started.
    pub count: u64,
    pub last_ms: Option<f64>,
    /// Over the last [`KEEP_LATENCIES`] seeks.
    pub p50_ms: Option<f64>,
    pub p95_ms: Option<f64>,
}

/// What `get_preview_stats` returns.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct PreviewDebug {
    pub backend: &'static str,
    /// Whether a preview engine is running.
    pub engine: bool,
    pub presented: u64,
    pub dropped: u64,
    pub seek: SeekSummary,
}

/// Seek latencies from the engine's `preview_seek` span.
#[derive(Debug, Default)]
pub struct SeekLatencies {
    samples: Mutex<VecDeque<u64>>,
    count: AtomicU64,
}

/// Nearest-rank percentile `p` (0..=1) of sorted `v`.
fn percentile(sorted: &[u64], p: f64) -> Option<u64> {
    if sorted.is_empty() {
        return None;
    }
    let rank = (p * sorted.len() as f64).ceil() as usize;
    Some(sorted[rank.clamp(1, sorted.len()) - 1])
}

fn ms(us: u64) -> f64 {
    us as f64 / 1000.0
}

impl SeekLatencies {
    /// Record one seek's latency in microseconds; logs p50/p95 every
    /// [`LOG_EVERY`] seeks.
    pub fn push(&self, us: u64) {
        {
            let mut s = lock(&self.samples);
            if s.len() == KEEP_LATENCIES {
                s.pop_front();
            }
            s.push_back(us);
        }
        let n = self.count.fetch_add(1, Ordering::Relaxed) + 1;
        if n % LOG_EVERY == 0 {
            let s = self.summary();
            tracing::info!(
                seeks = n,
                p50_ms = s.p50_ms,
                p95_ms = s.p95_ms,
                "preview seek latency (command received -> frame presented)"
            );
        }
    }

    pub fn summary(&self) -> SeekSummary {
        let s = lock(&self.samples);
        let last = s.back().copied();
        let mut sorted: Vec<u64> = s.iter().copied().collect();
        drop(s);
        sorted.sort_unstable();
        SeekSummary {
            count: self.count.load(Ordering::Relaxed),
            last_ms: last.map(ms),
            p50_ms: percentile(&sorted, 0.5).map(ms),
            p95_ms: percentile(&sorted, 0.95).map(ms),
        }
    }
}

/// Collects `latency_us` recorded on `preview_seek` spans.
struct SeekLatencyLayer(Arc<SeekLatencies>);

struct LatencyField(Option<u64>);

impl Visit for LatencyField {
    fn record_u64(&mut self, field: &Field, value: u64) {
        if field.name() == "latency_us" {
            self.0 = Some(value);
        }
    }
    fn record_i64(&mut self, field: &Field, value: i64) {
        if field.name() == "latency_us" {
            self.0 = u64::try_from(value).ok();
        }
    }
    fn record_debug(&mut self, _: &Field, _: &dyn std::fmt::Debug) {}
}

impl<S> Layer<S> for SeekLatencyLayer
where
    S: tracing::Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_record(&self, _: &span::Id, values: &span::Record<'_>, _: Context<'_, S>) {
        let mut v = LatencyField(None);
        values.record(&mut v);
        if let Some(us) = v.0 {
            self.0.push(us);
        }
    }
}

/// A `tracing` layer feeding `latencies` from the engine's seek span. It
/// enables only that span, whatever `RUST_LOG` says.
pub fn seek_latency_layer<S>(latencies: Arc<SeekLatencies>) -> impl Layer<S>
where
    S: tracing::Subscriber + for<'a> LookupSpan<'a>,
{
    SeekLatencyLayer(latencies).with_filter(filter_fn(|m| m.is_span() && m.name() == SEEK_SPAN))
}

/// Hand the session's current project to the transport (after every
/// `kv://state`).
pub fn project_changed<R: Runtime>(app: &AppHandle<R>) {
    let (Some(transport), Some(state)) = (
        app.try_state::<Arc<Transport>>(),
        app.try_state::<AppState>(),
    ) else {
        return;
    };
    let Ok(session) = state.session() else { return };
    let (project, root) = (
        session.project().clone(),
        session.media_root().to_path_buf(),
    );
    drop(session);
    transport.request_sync(project, root);
}

// ---- Tauri commands -------------------------------------------------------

#[tauri::command]
pub fn play(transport: State<'_, Arc<Transport>>) -> TransportStatus {
    transport.play()
}

#[tauri::command]
pub fn pause(transport: State<'_, Arc<Transport>>) -> TransportStatus {
    transport.pause()
}

/// Move the playhead to `position` (microseconds), keeping the play state.
#[tauri::command]
pub fn seek(transport: State<'_, Arc<Transport>>, position: Micros) -> TransportStatus {
    tracing::debug!(?position, "seek");
    transport.seek(position)
}

#[tauri::command]
pub fn get_transport(transport: State<'_, Arc<Transport>>) -> TransportStatus {
    transport.status()
}

/// Counters and seek latency percentiles for the debug overlay.
#[tauri::command]
pub fn get_preview_stats(
    transport: State<'_, Arc<Transport>>,
    preview: State<'_, PreviewState>,
) -> PreviewDebug {
    transport.debug(preview.backend().name())
}

#[cfg(test)]
mod tests;
