//! The live preview engine: plays and seeks a project section, producing
//! composited frames for a [`FrameSink`] at the display cadence while
//! decoding ahead on worker threads. Platform independent; presenting to a
//! window is the sink's job (ADR 0007).
//!
//! - **Clock.** A [`Clock`] decides what is due: position = position at
//!   play + (clock now - clock at play). [`PreviewEngine::tick`] shows the
//!   frame due at that position if its layers are decoded. A frame that is
//!   not ready before the next one is due is dropped, never shown late, so
//!   playback never runs slow or drifts from the clock (and later the audio).
//! - **Decode-ahead.** One [`VideoReader`] per source file, opened at
//!   preview size and reused across segments, edits and seeks. Frames for
//!   the next [`PreviewConfig::prefetch`] (500 ms, guess) of the timeline are
//!   queued in display order and decoded by [`PreviewConfig::decode_threads`]
//!   workers in parallel: different files at once, each file's frames in
//!   order on its own reader.
//! - **Seek** cancels queued prefetch, queues the frame at the target first,
//!   and records the time until it is presented in a `preview_seek`
//!   `tracing` span (field `latency_us`, also in [`PreviewStats`]).
//! - **Editing while playing** ([`PreviewEngine::set_edit`] with a new
//!   snapshot) keeps the position and invalidates every decoded and queued
//!   frame (simple, fine for the MVP); readers stay open.
//! - **Preview size** ([`preview_size`]): the output size when the viewer
//!   shows it at 1:1 or larger, otherwise the output shape fitted into
//!   960x540, the proxy size that held 57-60 fps in the ADR 0007 spike.
//!   Sources are decoded straight at [`source_size_for`] that size.
//!
//! - **Title card.** The section's title is drawn over the opening frames
//!   with the same fades as export ([`crate::FramePlan::with_title`]). It
//!   is rasterised at the preview size and handed to the compositor
//!   ([`PreviewCompositor::set_overlay`]) only when its text changes.
//!
//! - **Audio.** With [`PreviewConfig::audio`] the engine renders the
//!   section's music bed for the transport, keeps it in step on play,
//!   pause, seek and edit, and takes its time from the transport's
//!   [`AudioClock`] so picture follows sound (see [`audio`]).
//!
//! Driving: the native layer calls [`PreviewEngine::tick`] once per vsync;
//! [`Pacer`] does it on a thread for hosts without one.

pub mod audio;
mod clock;
mod output;

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use kv_audio::{render_music_bed, MusicBedOptions, PcmAudio};
use kv_core::{Edl, MediaId, Micros, MusicRef, Project, SectionId, TitleCard};
use kv_media::{DecodeOptions, MediaBackend, Rational, VideoFrame, VideoReader};

pub use audio::{AudioClock, AudioOutput, AudioTransport, NullOutput};
pub use clock::{Clock, ManualClock, SystemClock};
pub use output::{CpuCompositor, FrameSink, PreviewCompositor, RenderedFrame};

use crate::{frame_pts, plan, source_size_for, title, CompositeError, Fit, PlanError};
use crate::{Overlay, OverlayId, MUSIC_FADE_OUT};

/// The proxy size the preview falls back to when the output is larger than
/// the viewer (ADR 0007: 960x540 held 57-60 fps on the CI Mac).
pub const PROXY_SIZE: (u32, u32) = (960, 540);

/// Errors from the preview engine.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum PreviewError {
    #[error("project has no section {0:?}")]
    UnknownSection(SectionId),
    #[error(transparent)]
    Plan(#[from] PlanError),
    #[error("invalid preview size {width}x{height}")]
    InvalidSize { width: u32, height: u32 },
    #[error(transparent)]
    Composite(#[from] CompositeError),
    #[cfg(feature = "gpu")]
    #[error(transparent)]
    Gpu(#[from] crate::gpu::GpuError),
}

/// The size to render the preview at for an `output` (export) size shown in
/// a `viewer` rectangle (physical pixels): `output` if it fits in the
/// viewer, else `output`'s shape fitted inside [`PROXY_SIZE`] (never larger
/// than `output`).
pub fn preview_size(output: (u32, u32), viewer: (u32, u32)) -> (u32, u32) {
    let (w, h) = output;
    if w <= viewer.0 && h <= viewer.1 {
        return output;
    }
    let (pw, ph) = PROXY_SIZE;
    if w <= pw && h <= ph {
        return output;
    }
    source_size_for(pw, ph, w, h, Fit::Fit)
}

/// Settings of a [`PreviewEngine`].
#[derive(Clone)]
pub struct PreviewConfig {
    /// Display cadence: output frames per second.
    pub fps: Rational,
    /// How sources of another aspect ratio are placed (as in export).
    pub fit: Fit,
    /// Directory the project's relative media paths are resolved against
    /// (the project file's directory).
    pub media_root: PathBuf,
    /// How far ahead of the playhead frames are decoded (guess: 500 ms).
    pub prefetch: Micros,
    /// Decode worker threads.
    pub decode_threads: usize,
    /// Longest a [`PreviewEngine::tick`] blocks waiting for the due frame.
    pub max_wait: Duration,
    /// Drives playback. Ignored when [`audio`](Self::audio) is set: the
    /// transport's [`AudioClock`] drives it instead.
    pub clock: Arc<dyn Clock>,
    /// Plays the section's music bed in sync (see [`audio`]); `None` for a
    /// silent preview (no device, tests).
    pub audio: Option<Arc<AudioTransport>>,
}

impl Default for PreviewConfig {
    fn default() -> Self {
        let threads = std::thread::available_parallelism().map_or(2, |n| n.get());
        PreviewConfig {
            fps: Rational::new(30, 1),
            fit: Fit::Fill,
            media_root: PathBuf::new(),
            prefetch: Micros(500_000),
            decode_threads: threads.clamp(1, 4),
            max_wait: Duration::from_millis(100),
            clock: Arc::new(SystemClock::new()),
            audio: None,
        }
    }
}

impl std::fmt::Debug for PreviewConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PreviewConfig")
            .field("fps", &self.fps)
            .field("fit", &self.fit)
            .field("media_root", &self.media_root)
            .field("prefetch", &self.prefetch)
            .field("decode_threads", &self.decode_threads)
            .field("max_wait", &self.max_wait)
            .field("audio", &self.audio.is_some())
            .finish_non_exhaustive()
    }
}

/// Counters for diagnostics and tests.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PreviewStats {
    /// Frames handed to the sink.
    pub presented: u64,
    /// Frames that were due during playback but not decoded in time.
    pub dropped: u64,
    /// Queued decodes cancelled (seek, edit, or left behind the playhead).
    pub cancelled: u64,
    /// Frames decoded by the workers.
    pub decoded: u64,
    /// Readers opened.
    pub readers_opened: u64,
    /// From the last `seek` call to its frame being presented.
    pub last_seek_latency: Option<Duration>,
}

/// What one [`PreviewEngine::tick`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tick {
    /// No edit, or an empty one.
    Idle,
    /// The due frame is already on screen.
    Unchanged,
    /// The due frame was not decoded within [`PreviewConfig::max_wait`].
    Waiting,
    /// Frame `index` of the section was presented.
    Presented(u64),
}

type Key = (MediaId, Micros);
type PositionCallback = Arc<dyn Fn(Micros) + Send + Sync>;

enum Slot {
    Pending,
    Ready(Arc<VideoFrame>),
    Failed,
}

struct Job {
    key: Key,
    path: PathBuf,
}

struct Edit {
    edl: Edl,
    title: Option<TitleCard>,
    sources: HashMap<MediaId, PathBuf>,
    len: u64,
    end: Micros,
}

struct PendingSeek {
    started: Instant,
    span: tracing::Span,
}

struct State {
    edit: Option<Edit>,
    playing: bool,
    /// Position when paused; position at `anchor` when playing.
    pos: Micros,
    /// Clock time at which playback (re)started from `pos`.
    anchor: Micros,
    /// Index of the frame on screen, for the current edit.
    presented: Option<u64>,
    /// Last frame presented in the current run (play/seek), or the frame
    /// before the run's first; frames skipped after it count as dropped.
    run_prev: i64,
    cache: HashMap<Key, Slot>,
    queue: VecDeque<Job>,
    readers: HashMap<PathBuf, Box<dyn VideoReader>>,
    busy: HashSet<PathBuf>,
    seek: Option<PendingSeek>,
    stats: PreviewStats,
    shutdown: bool,
}

impl State {
    fn position(&self, now: Micros) -> Micros {
        let end = self.edit.as_ref().map_or(Micros(0), |e| e.end);
        let p = if self.playing {
            self.pos
                .0
                .saturating_add(now.0.saturating_sub(self.anchor.0))
        } else {
            self.pos.0
        };
        Micros(p.clamp(0, end.0))
    }

    /// Drop queued work and decoded frames (a seek or a new edit).
    fn cancel_all(&mut self) {
        self.stats.cancelled += self.queue.len() as u64;
        self.queue.clear();
        self.cache.clear();
    }
}

struct Shared {
    state: Mutex<State>,
    /// A decode finished or a control changed: `tick` re-evaluates.
    changed: Condvar,
    /// Work was queued, a reader was freed, or shutdown.
    work: Condvar,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn notify(&self) {
        self.changed.notify_all();
        self.work.notify_all();
    }
}

fn lock<T: ?Sized>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Plays and seeks a project section into a [`FrameSink`]. See the module
/// docs. All methods take `&self`; share it in an `Arc` between the UI and
/// the thread that calls [`tick`](Self::tick).
pub struct PreviewEngine {
    shared: Arc<Shared>,
    config: PreviewConfig,
    size: (u32, u32),
    compositor: Mutex<Box<dyn PreviewCompositor>>,
    sink: Mutex<Box<dyn FrameSink>>,
    on_position: Mutex<Option<PositionCallback>>,
    tick_lock: Mutex<()>,
    /// Last rendered bed: (music path, length) and the PCM.
    bed: Mutex<Option<(PathBuf, Micros, Arc<PcmAudio>)>>,
    /// Text of the title image the compositor holds, if any.
    title_text: Mutex<Option<String>>,
    workers: Vec<JoinHandle<()>>,
}

impl PreviewEngine {
    /// An engine decoding with `media`, drawing with `compositor` (CPU or
    /// GPU, at the size from [`preview_size`]) and presenting to `sink`.
    /// Starts the decode workers.
    pub fn new(
        media: Arc<dyn MediaBackend>,
        compositor: Box<dyn PreviewCompositor>,
        sink: Box<dyn FrameSink>,
        mut config: PreviewConfig,
    ) -> Result<Self, PreviewError> {
        if let Some(audio) = &config.audio {
            config.clock = audio.clock();
        }
        plan(&Edl::default(), config.fps, &no_source_fps)?;
        let size = compositor.size();
        if size.0 == 0 || size.1 == 0 {
            return Err(PreviewError::InvalidSize {
                width: size.0,
                height: size.1,
            });
        }
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                edit: None,
                playing: false,
                pos: Micros(0),
                anchor: Micros(0),
                presented: None,
                run_prev: -1,
                cache: HashMap::new(),
                queue: VecDeque::new(),
                readers: HashMap::new(),
                busy: HashSet::new(),
                seek: None,
                stats: PreviewStats::default(),
                shutdown: false,
            }),
            changed: Condvar::new(),
            work: Condvar::new(),
        });
        let workers = (0..config.decode_threads.max(1))
            .map(|i| {
                let shared = Arc::clone(&shared);
                let media = Arc::clone(&media);
                let fit = config.fit;
                std::thread::Builder::new()
                    .name(format!("kv-preview-decode-{i}"))
                    .spawn(move || worker(&shared, media.as_ref(), size, fit))
                    .expect("spawn preview decode worker")
            })
            .collect();
        Ok(PreviewEngine {
            shared,
            config,
            size,
            compositor: Mutex::new(compositor),
            sink: Mutex::new(sink),
            on_position: Mutex::new(None),
            tick_lock: Mutex::new(()),
            bed: Mutex::new(None),
            title_text: Mutex::new(None),
            workers,
        })
    }

    /// Preview size in pixels (the compositor's).
    pub fn size(&self) -> (u32, u32) {
        self.size
    }

    /// Show `section` of a project snapshot. Keeps the position (clamped to
    /// the new length) and the play state; invalidates every decoded and
    /// queued frame. Media missing from the project draw as black.
    pub fn set_edit(&self, project: &Project, section: SectionId) -> Result<(), PreviewError> {
        let sec = project
            .section(section)
            .ok_or(PreviewError::UnknownSection(section))?;
        let edl = sec.edit.clone();
        let p = plan(&edl, self.config.fps, &no_source_fps)?;
        let len = p.len();
        let end = Micros(
            edl.segments
                .iter()
                .map(crate::segment_end)
                .max()
                .unwrap_or(0),
        );
        let mut sources = HashMap::new();
        for seg in &edl.segments {
            match project.media_item(seg.media_id) {
                Some(item) => {
                    sources.insert(item.id, resolve(&self.config.media_root, &item.path));
                }
                None => tracing::warn!(media_id = seg.media_id.0, "preview: media not in project"),
            }
        }
        self.update_title(sec.title.as_ref())?;
        if let Some(audio) = &self.config.audio {
            self.update_bed(audio, project, sec.music.as_ref(), end);
        }
        let now = self.config.clock.now();
        let pos = {
            let mut st = self.shared.lock();
            let pos = Micros(st.position(now).0.min(end.0));
            st.pos = pos;
            st.anchor = now;
            st.cancel_all();
            let wanted: HashSet<&PathBuf> = sources.values().collect();
            st.readers.retain(|path, _| wanted.contains(path));
            st.edit = Some(Edit {
                edl,
                title: sec.title.clone(),
                sources,
                len,
                end,
            });
            st.presented = None;
            st.run_prev = index_at(self.config.fps, pos, len).map_or(-1, |k| k as i64 - 1);
            if st.edit.as_ref().is_some_and(|e| e.end <= pos) {
                st.playing = false;
            }
            self.sync_audio(&st, now);
            self.schedule(&mut st, pos);
            pos
        };
        self.shared.notify();
        self.report_position(pos);
        Ok(())
    }

    /// Start playing from the current position (from the start if at the
    /// end).
    pub fn play(&self) {
        let now = self.config.clock.now();
        {
            let mut st = self.shared.lock();
            let Some(edit) = &st.edit else { return };
            if st.playing || edit.len == 0 {
                return;
            }
            let (end, len) = (edit.end, edit.len);
            if st.pos >= end {
                st.pos = Micros(0);
            }
            st.playing = true;
            st.anchor = now;
            let k = index_at(self.config.fps, st.pos, len).unwrap_or(0);
            st.run_prev = if st.presented == Some(k) {
                k as i64
            } else {
                k as i64 - 1
            };
            self.sync_audio(&st, now);
        }
        self.shared.notify();
    }

    /// Stop at the current position.
    pub fn pause(&self) {
        let now = self.config.clock.now();
        {
            let mut st = self.shared.lock();
            st.pos = st.position(now);
            st.playing = false;
            self.sync_audio(&st, now);
        }
        self.shared.notify();
    }

    /// True while playing.
    pub fn is_playing(&self) -> bool {
        self.shared.lock().playing
    }

    /// Jump to `t` (clamped to the edit), keeping the play state. Cancels
    /// queued prefetch; the frame at `t` is presented as soon as it is
    /// decoded.
    pub fn seek(&self, t: Micros) {
        let now = self.config.clock.now();
        let pos = {
            let mut st = self.shared.lock();
            let Some(edit) = &st.edit else { return };
            let (end, len) = (edit.end, edit.len);
            let pos = Micros(t.0.clamp(0, end.0));
            st.pos = pos;
            st.anchor = now;
            st.cancel_all();
            st.presented = None;
            st.run_prev = index_at(self.config.fps, pos, len).map_or(-1, |k| k as i64 - 1);
            st.seek = Some(PendingSeek {
                started: Instant::now(),
                span: tracing::info_span!(
                    "preview_seek",
                    target_us = pos.0,
                    latency_us = tracing::field::Empty
                ),
            });
            self.sync_audio(&st, now);
            self.schedule(&mut st, pos);
            pos
        };
        self.shared.notify();
        self.report_position(pos);
    }

    /// Current playhead position on the section timeline.
    pub fn position(&self) -> Micros {
        self.shared.lock().position(self.config.clock.now())
    }

    /// Call `f` with the playhead position whenever a frame is presented
    /// and on every seek or edit (for the UI playhead). Replaces any
    /// previous callback. Runs on the calling thread of `tick`/`seek`.
    pub fn on_position(&self, f: impl Fn(Micros) + Send + Sync + 'static) {
        *lock(&self.on_position) = Some(Arc::new(f));
    }

    /// Counters since the engine was created.
    pub fn stats(&self) -> PreviewStats {
        self.shared.lock().stats
    }

    /// One pacing step: show the frame due now if it is decoded.
    ///
    /// Reads the clock, queues decode-ahead, and if the due frame is not on
    /// screen waits for its layers (up to [`PreviewConfig::max_wait`]). If
    /// the clock moves on to the next frame meanwhile, the late one is
    /// dropped. Call it once per display refresh.
    pub fn tick(&self) -> Result<Tick, PreviewError> {
        let _one_at_a_time = lock(&self.tick_lock);
        let deadline = Instant::now() + self.config.max_wait;
        let mut st = self.shared.lock();
        let (index, pts, (frames, overlays)) = loop {
            let now = self.config.clock.now();
            let Some(edit) = &st.edit else {
                return Ok(Tick::Idle);
            };
            let (end, len) = (edit.end, edit.len);
            let mut pos = st.position(now);
            if st.playing && pos >= end {
                st.playing = false;
                st.pos = end;
                pos = end;
                self.sync_audio(&st, now);
            }
            let Some(k) = index_at(self.config.fps, pos, len) else {
                return Ok(Tick::Idle);
            };
            self.schedule(&mut st, pos);
            if st.presented == Some(k) {
                return Ok(Tick::Unchanged);
            }
            if let Some(frames) = ready_layers(&st, self.config.fps, k) {
                break (k, Micros(frame_pts(k, self.config.fps)), frames);
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Ok(Tick::Waiting);
            }
            // Poll so a moving clock is noticed even without a decode.
            let wait = left.min(Duration::from_millis(2));
            st = self
                .shared
                .changed
                .wait_timeout(st, wait)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        };
        drop(st);

        let layers: Vec<(&VideoFrame, f32)> =
            frames.iter().map(|(f, w)| (f.as_ref(), *w)).collect();
        let rendered = {
            let mut compositor = lock(&self.compositor);
            match compositor.render_with_overlays(pts, &layers, &overlays, self.config.fit) {
                Ok(frame) => {
                    lock(&self.sink).present(&frame);
                    Ok(())
                }
                Err(e) => Err(e),
            }
        };

        let pos = {
            let mut st = self.shared.lock();
            // Even a failed render counts as shown, so it is not retried
            // every tick.
            st.presented = Some(index);
            let k = index as i64;
            if k > st.run_prev + 1 {
                st.stats.dropped += (k - st.run_prev - 1) as u64;
            }
            st.run_prev = st.run_prev.max(k);
            if rendered.is_ok() {
                st.stats.presented += 1;
            }
            if let Some(seek) = st.seek.take() {
                let latency = seek.started.elapsed();
                st.stats.last_seek_latency = Some(latency);
                let us = u64::try_from(latency.as_micros()).unwrap_or(u64::MAX);
                seek.span.record("latency_us", us);
                let _entered = seek.span.enter();
                tracing::info!(latency_us = us, frame = index, "preview seek frame shown");
            }
            st.position(self.config.clock.now())
        };
        rendered?;
        self.report_position(pos);
        Ok(Tick::Presented(index))
    }

    /// Rasterise `title` at the preview size and give it to the compositor,
    /// or clear it, unless the compositor already holds that text.
    fn update_title(&self, title: Option<&TitleCard>) -> Result<(), PreviewError> {
        let text = title.map(|t| t.text.as_str());
        let mut held = lock(&self.title_text);
        if held.as_deref() == text {
            return Ok(());
        }
        let image = title.map(|t| title::rasterize(t, self.size.0, self.size.1));
        lock(&self.compositor).set_overlay(OverlayId::Title, image.as_ref())?;
        *held = text.map(str::to_owned);
        Ok(())
    }

    /// Point the audio transport at the playhead: bed frame `st.pos` is
    /// heard at clock `now` (the anchor just set), playing or not.
    fn sync_audio(&self, st: &State, now: Micros) {
        if let Some(audio) = &self.config.audio {
            audio.sync(st.playing, st.pos, now);
        }
    }

    /// Render (or reuse) the music bed for a section of length `end` and
    /// hand it to `audio` with the project's music volume. The bed is
    /// rendered at 100%; the volume is applied live by the transport. A
    /// music file that cannot be read plays as silence.
    fn update_bed(
        &self,
        audio: &AudioTransport,
        project: &Project,
        music: Option<&MusicRef>,
        end: Micros,
    ) {
        audio.set_volume(f32::from(project.settings.music_volume_percent) / 100.0);
        let bed = music.and_then(|m| {
            let path = resolve(&self.config.media_root, &m.path);
            let mut cache = lock(&self.bed);
            if let Some((p, len, bed)) = cache.as_ref() {
                if *p == path && *len == end {
                    return Some(Arc::clone(bed));
                }
            }
            let mut opts = MusicBedOptions::new(audio.sample_rate(), audio.channels(), end);
            opts.fade_out = MUSIC_FADE_OUT;
            match render_music_bed(&path, &opts) {
                Ok(pcm) => {
                    let bed = Arc::new(pcm);
                    *cache = Some((path, end, Arc::clone(&bed)));
                    Some(bed)
                }
                Err(e) => {
                    tracing::warn!(path = %path.display(), "preview: cannot render music: {e}");
                    None
                }
            }
        });
        if let Err(e) = audio.set_bed(bed) {
            tracing::warn!("preview: {e}");
        }
    }

    fn report_position(&self, pos: Micros) {
        let cb = lock(&self.on_position).clone();
        if let Some(cb) = cb {
            cb(pos);
        }
    }

    /// Queue the decodes for the window starting at `pos`, in display
    /// order, and forget frames and jobs outside it.
    fn schedule(&self, st: &mut State, pos: Micros) {
        let Some(edit) = &st.edit else { return };
        let Ok(p) = plan(&edit.edl, self.config.fps, &no_source_fps) else {
            return;
        };
        let Some(first) = index_at(self.config.fps, pos, edit.len) else {
            return;
        };
        let window = window_frames(self.config.prefetch, self.config.fps);
        let mut needed = Vec::new();
        let mut seen = HashSet::new();
        for k in first..first.saturating_add(window).min(edit.len) {
            let Some(frame) = p.frame(k) else { break };
            for l in frame.layers {
                let key = (l.media_id, l.src_time);
                if seen.insert(key) {
                    needed.push((key, edit.sources.get(&l.media_id).cloned()));
                }
            }
        }
        let before = st.queue.len();
        st.queue.retain(|j| seen.contains(&j.key));
        st.stats.cancelled += (before - st.queue.len()) as u64;
        st.cache.retain(|k, _| seen.contains(k));
        let mut queued = false;
        for (key, path) in needed {
            if st.cache.contains_key(&key) {
                continue;
            }
            match path {
                Some(path) => {
                    st.cache.insert(key, Slot::Pending);
                    st.queue.push_back(Job { key, path });
                    queued = true;
                }
                None => {
                    st.cache.insert(key, Slot::Failed);
                }
            }
        }
        if queued {
            self.shared.work.notify_all();
        }
    }
}

impl Drop for PreviewEngine {
    fn drop(&mut self) {
        self.shared.lock().shutdown = true;
        self.shared.notify();
        for w in self.workers.drain(..) {
            let _ = w.join();
        }
    }
}

/// A frame ready to draw: decoded layers (frame, weight) and overlays.
type ReadyFrame = (Vec<(Arc<VideoFrame>, f32)>, Vec<Overlay>);

/// The decoded layers of frame `k` with their weights and its overlays, or
/// `None` while any layer is still pending. Failed layers are left out
/// (drawn black).
fn ready_layers(st: &State, fps: Rational, k: u64) -> Option<ReadyFrame> {
    let edit = st.edit.as_ref()?;
    let frame = plan(&edit.edl, fps, &no_source_fps)
        .ok()?
        .with_title(edit.title.as_ref())
        .frame(k)?;
    let mut out = Vec::with_capacity(frame.layers.len());
    for l in frame.layers {
        match st.cache.get(&(l.media_id, l.src_time)) {
            Some(Slot::Ready(f)) => out.push((Arc::clone(f), l.weight)),
            Some(Slot::Failed) => {}
            Some(Slot::Pending) | None => return None,
        }
    }
    Some((out, frame.overlays))
}

/// Source frame rates for the plan: not known here (sources are probed
/// lazily by the decode workers), so the preview shows the nearest source
/// frame without slow-mo blending for now.
fn no_source_fps(_: MediaId) -> Option<Rational> {
    None
}

/// Frames in the prefetch window: `ceil(prefetch * fps)`, at least 1.
fn window_frames(prefetch: Micros, fps: Rational) -> u64 {
    let n = i128::from(prefetch.0.max(0)) * i128::from(fps.num);
    let d = 1_000_000 * i128::from(fps.den);
    u64::try_from((n + d - 1) / d).unwrap_or(u64::MAX).max(1)
}

/// The index of the frame displayed at `t` (the last with `pts <= t`),
/// clamped to the plan; `None` for an empty plan.
fn index_at(fps: Rational, t: Micros, len: u64) -> Option<u64> {
    if len == 0 {
        return None;
    }
    let t = t.0.max(0);
    let est = i128::from(t) * i128::from(fps.num) / (1_000_000 * i128::from(fps.den));
    let mut k = u64::try_from(est).unwrap_or(u64::MAX).min(len - 1);
    while k > 0 && frame_pts(k, fps) > t {
        k -= 1;
    }
    while k + 1 < len && frame_pts(k + 1, fps) <= t {
        k += 1;
    }
    Some(k)
}

/// `root / rel`, where `rel` is `/`-separated (an absolute `rel` wins).
fn resolve(root: &Path, rel: &str) -> PathBuf {
    rel.split('/').filter(|p| !p.is_empty()).fold(
        if rel.starts_with('/') {
            PathBuf::from("/")
        } else {
            root.to_path_buf()
        },
        |acc, part| acc.join(part),
    )
}

/// Decode worker: takes the earliest queued job whose file's reader is
/// free, decodes it outside the lock, stores the frame.
fn worker(shared: &Shared, media: &dyn MediaBackend, size: (u32, u32), fit: Fit) {
    let mut st = shared.lock();
    loop {
        if st.shutdown {
            return;
        }
        let Some(i) = st.queue.iter().position(|j| !st.busy.contains(&j.path)) else {
            st = shared.work.wait(st).unwrap_or_else(PoisonError::into_inner);
            continue;
        };
        let job = st.queue.remove(i).expect("index from position");
        if !matches!(st.cache.get(&job.key), Some(Slot::Pending)) {
            continue; // already decoded by an earlier, re-queued job
        }
        st.busy.insert(job.path.clone());
        let reader = st.readers.remove(&job.path);
        drop(st);

        let opened = reader.is_none();
        let (reader, slot) = decode(media, reader, &job, size, fit);

        st = shared.lock();
        st.busy.remove(&job.path);
        if opened && reader.is_some() {
            st.stats.readers_opened += 1;
        }
        if let Some(r) = reader {
            let wanted = st
                .edit
                .as_ref()
                .is_some_and(|e| e.sources.values().any(|p| *p == job.path));
            if wanted {
                st.readers.insert(job.path.clone(), r);
            }
        }
        let ready = matches!(slot, Slot::Ready(_));
        if let Some(s @ Slot::Pending) = st.cache.get_mut(&job.key) {
            *s = slot;
            if ready {
                st.stats.decoded += 1;
            }
        }
        shared.notify();
    }
}

fn decode(
    media: &dyn MediaBackend,
    reader: Option<Box<dyn VideoReader>>,
    job: &Job,
    size: (u32, u32),
    fit: Fit,
) -> (Option<Box<dyn VideoReader>>, Slot) {
    let (media_id, t) = job.key;
    let mut reader = match reader {
        Some(r) => r,
        None => match open(media, &job.path, size, fit) {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(media_id = media_id.0, path = %job.path.display(), "preview: cannot open source: {e}");
                return (None, Slot::Failed);
            }
        },
    };
    let frame = match reader.frame_at(t) {
        Ok(Some(f)) => Ok(Some(f)),
        // `t` before the first frame (a stream starting late): show the
        // first frame, as export does.
        Ok(None) => reader.seek(Micros(0)).and_then(|()| reader.next_frame()),
        Err(e) => Err(e),
    };
    let slot = match frame {
        Ok(Some(f)) => Slot::Ready(Arc::new(f)),
        Ok(None) => {
            tracing::warn!(media_id = media_id.0, t = t.0, "preview: no frame");
            Slot::Failed
        }
        Err(e) => {
            tracing::warn!(
                media_id = media_id.0,
                t = t.0,
                "preview: decode failed: {e}"
            );
            Slot::Failed
        }
    };
    (Some(reader), slot)
}

/// Open `path` decoding upright at the size the compositor needs.
fn open(
    media: &dyn MediaBackend,
    path: &Path,
    size: (u32, u32),
    fit: Fit,
) -> kv_media::Result<Box<dyn VideoReader>> {
    let info = media.probe(path)?;
    let video = info
        .video
        .ok_or_else(|| kv_media::MediaError::NoVideoStream(path.to_path_buf()))?;
    let (w, h) = if video.rotation % 180 == 90 {
        (video.height, video.width)
    } else {
        (video.width, video.height)
    };
    let dec = source_size_for(size.0, size.1, w, h, fit);
    media.open_video(
        path,
        &DecodeOptions {
            size: (dec != (0, 0)).then_some(dec),
            apply_rotation: true,
        },
    )
}

/// Calls [`PreviewEngine::tick`] on a thread, for hosts without a vsync
/// callback. Stops when dropped.
pub struct Pacer {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Pacer {
    /// Tick `engine` about four times per frame interval until dropped.
    pub fn spawn(engine: Arc<PreviewEngine>) -> Pacer {
        let stop = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&stop);
        let fps = engine.config.fps;
        let interval =
            Duration::from_micros(u64::try_from(frame_pts(1, fps)).unwrap_or(33_333).max(4) / 4);
        let thread = std::thread::Builder::new()
            .name("kv-preview-pacer".into())
            .spawn(move || {
                while !flag.load(Ordering::Relaxed) {
                    match engine.tick() {
                        Ok(Tick::Presented(_)) => {}
                        Ok(_) => std::thread::sleep(interval),
                        Err(e) => {
                            tracing::warn!("preview: {e}");
                            std::thread::sleep(interval);
                        }
                    }
                }
            })
            .expect("spawn preview pacer");
        Pacer {
            stop,
            thread: Some(thread),
        }
    }
}

impl Drop for Pacer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

#[cfg(test)]
mod tests;
