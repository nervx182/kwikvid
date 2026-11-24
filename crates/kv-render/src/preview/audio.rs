//! Preview sound: the section's music bed played through an [`AudioOutput`],
//! with the audio device as the master clock.
//!
//! - **Output.** [`AudioOutput`] is the device: [`CpalOutput`] (the default
//!   output device via `cpal`; built on macOS, elsewhere with the `cpal`
//!   feature) or [`NullOutput`] (discards everything; headless CI, tests,
//!   machines without a device).
//! - **What plays.** [`PreviewAudio::set_section`] renders the section's
//!   music with [`kv_audio::render_music_bed`] at the device's format, for
//!   the length of the edit, with the export's fade-out
//!   ([`crate::MUSIC_FADE_OUT`]), at 100%. The project's music volume is a
//!   live gain applied while playing ([`PreviewAudio::set_volume`]), so a
//!   volume change is heard on the next device buffer; gain then clamp is
//!   what export does, so the levels match.
//! - **Following the engine.** [`PreviewAudio::attach`] subscribes to the
//!   engine's [`Transport`]. The device callback ([`AudioRenderer::render`])
//!   works out, for every sample it writes, which timeline position will be
//!   on screen when that sample is heard, and copies the bed there. A seek,
//!   play or pause is picked up by the next buffer (a few ms at typical
//!   buffer sizes, well inside 50 ms), and sync holds by construction rather
//!   than by correcting drift.
//! - **Clock.** [`AudioClock`] is the engine's [`Clock`]. With a device it
//!   reads the frames played so far: frames written minus the device latency
//!   (`playback - callback` from the device timestamps where available),
//!   interpolated with system time between callbacks so the video sees a
//!   smooth clock. Without a device clock ([`NullOutput`]) it is system time.
//!
//! Clip audio is not mixed yet: TODO(#72) mix `render_clip_audio` into the
//! bed the way export does once it exists.

#[cfg(any(feature = "cpal", target_os = "macos"))]
mod cpal_out;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use kv_audio::{render_music_bed, MusicBedOptions, PcmAudio};
use kv_core::{Micros, Project, SectionId};

#[cfg(any(feature = "cpal", target_os = "macos"))]
pub use cpal_out::CpalOutput;

use super::{Clock, PreviewEngine, SystemClock, Transport};

/// Sample format the preview audio is rendered in: interleaved `f32`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioFormat {
    /// Frames per second (Hz). Must be > 0.
    pub sample_rate: u32,
    /// Channels per frame: 1 or 2.
    pub channels: u16,
}

impl AudioFormat {
    /// 48 kHz stereo, what [`NullOutput::new`] reports.
    pub const DEFAULT: AudioFormat = AudioFormat {
        sample_rate: 48_000,
        channels: 2,
    };
}

/// Errors from the preview audio.
#[derive(Debug, thiserror::Error)]
pub enum AudioError {
    #[error("no audio output device")]
    NoDevice,
    #[error("audio device: {0}")]
    Device(String),
    #[error("unsupported audio format {0:?}")]
    InvalidFormat(AudioFormat),
    #[error("the audio output was already started")]
    AlreadyStarted,
    #[error("project has no section {0:?}")]
    UnknownSection(SectionId),
    #[error("music {path:?}: {source}")]
    Music {
        path: PathBuf,
        #[source]
        source: kv_audio::Error,
    },
}

/// An audio device the preview plays through.
pub trait AudioOutput: Send {
    /// The format the device wants [`AudioRenderer::render`] to produce.
    fn format(&self) -> AudioFormat;

    /// True when the device pulls samples in real time, so the clock can
    /// follow the frames it plays. False: the clock falls back to system
    /// time.
    fn has_device_clock(&self) -> bool;

    /// Start pulling from `renderer` (typically from the device's own
    /// thread) until the output is dropped. Called once.
    fn start(&mut self, renderer: AudioRenderer) -> Result<(), AudioError>;
}

/// An output without a device: keeps the renderer but never pulls from it,
/// and reports no device clock (the preview runs on system time).
pub struct NullOutput {
    format: AudioFormat,
    renderer: Option<AudioRenderer>,
}

impl NullOutput {
    /// A null output reporting [`AudioFormat::DEFAULT`].
    pub fn new() -> Self {
        NullOutput {
            format: AudioFormat::DEFAULT,
            renderer: None,
        }
    }
}

impl Default for NullOutput {
    fn default() -> Self {
        Self::new()
    }
}

impl AudioOutput for NullOutput {
    fn format(&self) -> AudioFormat {
        self.format
    }

    fn has_device_clock(&self) -> bool {
        false
    }

    fn start(&mut self, renderer: AudioRenderer) -> Result<(), AudioError> {
        if self.renderer.is_some() {
            return Err(AudioError::AlreadyStarted);
        }
        self.renderer = Some(renderer);
        Ok(())
    }
}

/// The preview's master [`Clock`]: frames played by the audio device, or
/// system time when there is no device. See the module docs.
pub struct AudioClock {
    device: Option<Arc<DeviceClock>>,
    fallback: Arc<dyn Clock>,
}

impl AudioClock {
    /// True when driven by the device, false when it is system time.
    pub fn is_device(&self) -> bool {
        self.device.is_some()
    }
}

impl Clock for AudioClock {
    fn now(&self) -> Micros {
        match &self.device {
            Some(d) => d.now(),
            None => self.fallback.now(),
        }
    }
}

/// Device-driven time. The device callback reports each buffer
/// ([`DeviceClock::buffer`]); `now` is the frame being heard at the last
/// callback plus the reference time since, capped at that buffer's length.
struct DeviceClock {
    rate: u32,
    reference: Arc<dyn Clock>,
    /// Frame heard at the last callback (frames written before it minus the
    /// latency), as clock micros.
    base: AtomicI64,
    /// Reference time of the last callback.
    ref_at: AtomicI64,
    /// Length of the last buffer, in micros (interpolation cap).
    period: AtomicI64,
    /// Largest value returned, so `now` never goes backwards.
    last: AtomicI64,
}

impl DeviceClock {
    fn now(&self) -> Micros {
        let base = self.base.load(Ordering::SeqCst);
        let since = self
            .reference
            .now()
            .0
            .saturating_sub(self.ref_at.load(Ordering::SeqCst));
        let t = base.saturating_add(since.clamp(0, self.period.load(Ordering::SeqCst)));
        Micros(self.last.fetch_max(t, Ordering::SeqCst).max(t))
    }

    /// The device is about to play `frames` frames starting at frame
    /// `written` (frames written before this buffer), the first heard after
    /// `latency`.
    fn buffer(&self, written: u64, frames: u64, latency: Micros) {
        let heard = written.saturating_sub(frames_at(latency, self.rate));
        self.ref_at.store(self.reference.now().0, Ordering::SeqCst);
        self.period
            .store(micros_at(frames, self.rate), Ordering::SeqCst);
        self.base
            .store(micros_at(heard, self.rate), Ordering::SeqCst);
    }
}

/// `round(t * rate)` frames, 0 for negative `t`.
fn frames_at(t: Micros, rate: u32) -> u64 {
    let n = i128::from(t.0.max(0)) * i128::from(rate);
    u64::try_from((n + 500_000) / 1_000_000).unwrap_or(u64::MAX)
}

/// Signed `round(t * rate)` frames.
fn frames_signed(t: Micros, rate: u32) -> i128 {
    let n = i128::from(t.0) * i128::from(rate);
    if n >= 0 {
        (n + 500_000) / 1_000_000
    } else {
        (n - 500_000) / 1_000_000
    }
}

/// Start time of frame `frame`: `floor(frame / rate)` in micros.
fn micros_at(frame: u64, rate: u32) -> i64 {
    i64::try_from(i128::from(frame) * 1_000_000 / i128::from(rate)).unwrap_or(i64::MAX)
}

/// A [`Transport`] the audio thread reads without blocking (a sequence
/// lock: writers are serialised by a mutex, the reader retries on a torn
/// read).
#[derive(Default)]
struct TransportCell {
    seq: AtomicU64,
    playing: AtomicBool,
    pos: AtomicI64,
    anchor: AtomicI64,
    end: AtomicI64,
    write: Mutex<()>,
}

impl TransportCell {
    fn store(&self, t: Transport) {
        let _one_writer = lock(&self.write);
        self.seq.fetch_add(1, Ordering::SeqCst);
        self.playing.store(t.playing, Ordering::SeqCst);
        self.pos.store(t.pos.0, Ordering::SeqCst);
        self.anchor.store(t.anchor.0, Ordering::SeqCst);
        self.end.store(t.end.0, Ordering::SeqCst);
        self.seq.fetch_add(1, Ordering::SeqCst);
    }

    fn load(&self) -> Transport {
        loop {
            let before = self.seq.load(Ordering::SeqCst);
            if before % 2 == 1 {
                std::hint::spin_loop();
                continue;
            }
            let t = Transport {
                playing: self.playing.load(Ordering::SeqCst),
                pos: Micros(self.pos.load(Ordering::SeqCst)),
                anchor: Micros(self.anchor.load(Ordering::SeqCst)),
                end: Micros(self.end.load(Ordering::SeqCst)),
            };
            if self.seq.load(Ordering::SeqCst) == before {
                return t;
            }
        }
    }
}

/// State shared by [`PreviewAudio`] (control side) and the
/// [`AudioRenderer`] (device side).
struct Shared {
    format: AudioFormat,
    transport: TransportCell,
    /// Live music gain, `f32` bits.
    gain: AtomicU32,
    /// The bed (interleaved, `format`), swapped whole on change.
    bed: Mutex<Arc<Vec<f32>>>,
    /// Bumped on every bed swap so the renderer only locks when needed.
    bed_gen: AtomicU64,
}

/// The device side: fills device buffers with the bed at the positions the
/// engine will show when they are heard. Owned by the device's callback.
pub struct AudioRenderer {
    shared: Arc<Shared>,
    clock: Option<Arc<DeviceClock>>,
    bed: Arc<Vec<f32>>,
    bed_gen: u64,
    /// Frames written so far.
    written: u64,
}

impl AudioRenderer {
    /// The format [`render`](Self::render) produces.
    pub fn format(&self) -> AudioFormat {
        self.shared.format
    }

    /// Fill `out` (interleaved, [`format`](Self::format); a trailing partial
    /// frame is left silent) with the next samples. `latency` is how long
    /// until the first of them is heard (0 if the device cannot tell).
    pub fn render(&mut self, out: &mut [f32], latency: Micros) {
        let format = self.shared.format;
        let ch = usize::from(format.channels);
        let rate = format.sample_rate;
        let frames = out.len() / ch;
        out.fill(0.0);

        let gen = self.shared.bed_gen.load(Ordering::SeqCst);
        if gen != self.bed_gen {
            // Never block the device thread: retry on the next buffer.
            if let Ok(bed) = self.shared.bed.try_lock() {
                self.bed = Arc::clone(&bed);
                self.bed_gen = gen;
            }
        }

        let t = self.shared.transport.load();
        let w0 = i128::from(self.written);
        if t.playing {
            let gain = f32::from_bits(self.shared.gain.load(Ordering::SeqCst));
            // Sample `w` is heard at clock time `w / rate`, when the engine
            // shows `pos + (w / rate - anchor)`: bed frame `w + delta`, from
            // the first sample heard at or after `anchor`.
            let first = (frames_signed(t.anchor, rate) - w0).max(0);
            let delta = frames_signed(t.pos, rate) - frames_signed(t.anchor, rate);
            let bed_frames = (self.bed.len() / ch) as i128;
            let end = (frames_signed(t.end, rate)).min(bed_frames);
            for i in first..frames as i128 {
                let f = w0 + i + delta;
                if f < 0 {
                    continue;
                }
                if f >= end {
                    break;
                }
                let (i, f) = (i as usize * ch, f as usize * ch);
                for (o, s) in out[i..i + ch].iter_mut().zip(&self.bed[f..f + ch]) {
                    *o = (s * gain).clamp(-1.0, 1.0);
                }
            }
        }

        if let Some(clock) = &self.clock {
            clock.buffer(self.written, frames as u64, latency);
        }
        self.written += frames as u64;
    }
}

/// What the current bed was rendered from, to skip re-rendering when an
/// edit leaves the music and length unchanged.
#[derive(Debug, Clone, PartialEq, Eq)]
struct BedKey {
    music: Option<PathBuf>,
    length: Micros,
}

/// Plays a section's music in sync with a [`PreviewEngine`]. See the module
/// docs. Typical use:
///
/// ```ignore
/// let audio = PreviewAudio::open_default();
/// let engine = PreviewEngine::new(media, compositor, sink,
///     PreviewConfig { clock: audio.clock(), ..config })?;
/// audio.attach(&engine);
/// audio.set_section(&project, section, &media_root)?; // after each edit
/// ```
pub struct PreviewAudio {
    shared: Arc<Shared>,
    clock: Arc<AudioClock>,
    bed_key: Mutex<Option<BedKey>>,
    output: Mutex<Box<dyn AudioOutput>>,
}

impl PreviewAudio {
    /// Play through `output`; between device callbacks the clock
    /// interpolates with system time.
    pub fn new(output: Box<dyn AudioOutput>) -> Result<Self, AudioError> {
        Self::with_reference(output, Arc::new(SystemClock::new()))
    }

    /// Like [`new`](Self::new), with `reference` as the system time (the
    /// fallback clock and the interpolation between callbacks).
    pub fn with_reference(
        mut output: Box<dyn AudioOutput>,
        reference: Arc<dyn Clock>,
    ) -> Result<Self, AudioError> {
        let format = output.format();
        if format.sample_rate == 0 || !(1..=2).contains(&format.channels) {
            return Err(AudioError::InvalidFormat(format));
        }
        let device = output.has_device_clock().then(|| {
            Arc::new(DeviceClock {
                rate: format.sample_rate,
                reference: Arc::clone(&reference),
                base: AtomicI64::new(0),
                ref_at: AtomicI64::new(reference.now().0),
                period: AtomicI64::new(0),
                last: AtomicI64::new(0),
            })
        });
        let shared = Arc::new(Shared {
            format,
            transport: TransportCell::default(),
            gain: AtomicU32::new(1.0f32.to_bits()),
            bed: Mutex::new(Arc::new(Vec::new())),
            bed_gen: AtomicU64::new(0),
        });
        output.start(AudioRenderer {
            shared: Arc::clone(&shared),
            clock: device.clone(),
            bed: Arc::new(Vec::new()),
            bed_gen: 0,
            written: 0,
        })?;
        Ok(PreviewAudio {
            shared,
            clock: Arc::new(AudioClock {
                device,
                fallback: reference,
            }),
            bed_key: Mutex::new(None),
            output: Mutex::new(output),
        })
    }

    /// The default output device, or a [`NullOutput`] (system clock, no
    /// sound) when there is none or this build has no device support.
    pub fn open_default() -> Self {
        #[cfg(any(feature = "cpal", target_os = "macos"))]
        match CpalOutput::open_default().and_then(|o| Self::new(Box::new(o))) {
            Ok(audio) => return audio,
            Err(e) => tracing::warn!("preview audio: no output device, playing silent: {e}"),
        }
        Self::new(Box::new(NullOutput::new())).expect("null output starts")
    }

    /// The clock to give the engine ([`super::PreviewConfig::clock`]).
    pub fn clock(&self) -> Arc<AudioClock> {
        Arc::clone(&self.clock)
    }

    /// The device format the bed is rendered at.
    pub fn format(&self) -> AudioFormat {
        self.shared.format
    }

    /// True when sound goes to a real-time device (and drives the clock).
    pub fn has_device(&self) -> bool {
        self.clock.is_device()
    }

    /// Follow `engine`'s play, pause and seeks. The engine must use
    /// [`clock`](Self::clock).
    pub fn attach(&self, engine: &PreviewEngine) {
        let shared = Arc::clone(&self.shared);
        engine.on_transport(move |t| shared.transport.store(t));
    }

    /// Set the playback state directly (what [`attach`](Self::attach) wires
    /// up).
    pub fn set_transport(&self, t: Transport) {
        self.shared.transport.store(t);
    }

    /// Music volume in percent (as `ProjectSettings::music_volume_percent`),
    /// applied from the next device buffer.
    pub fn set_volume(&self, percent: u8) {
        let gain = f32::from(percent) / 100.0;
        self.shared.gain.store(gain.to_bits(), Ordering::SeqCst);
    }

    /// Play `section`'s music for its current edit at the project's music
    /// volume. Re-renders the bed (decodes the song: call it off the UI
    /// thread) only when the music file or the edit length changed;
    /// otherwise just updates the volume. Without music the preview is
    /// silent. `media_root` resolves the project's relative paths.
    pub fn set_section(
        &self,
        project: &Project,
        section: SectionId,
        media_root: &Path,
    ) -> Result<(), AudioError> {
        let sec = project
            .section(section)
            .ok_or(AudioError::UnknownSection(section))?;
        let length = Micros(
            sec.edit
                .segments
                .iter()
                .map(crate::segment_end)
                .max()
                .unwrap_or(0),
        );
        let key = BedKey {
            music: sec
                .music
                .as_ref()
                .map(|m| super::resolve(media_root, &m.path)),
            length,
        };
        self.set_volume(project.settings.music_volume_percent);
        let mut current = lock(&self.bed_key);
        if current.as_ref() == Some(&key) {
            return Ok(());
        }
        let format = self.shared.format;
        let samples = match &key.music {
            Some(path) => {
                let mut opts = MusicBedOptions::new(format.sample_rate, format.channels, length);
                // Volume is the live gain; fades as in export.
                opts.volume_percent = 100;
                opts.fade_out = crate::MUSIC_FADE_OUT;
                render_music_bed(path, &opts)
                    .map_err(|source| AudioError::Music {
                        path: path.clone(),
                        source,
                    })?
                    .samples
            }
            None => Vec::new(),
        };
        // TODO(#72): mix the clip audio (`render_clip_audio`) in here, as
        // export does, once it exists.
        self.swap_bed(samples);
        *current = Some(key);
        Ok(())
    }

    /// Play `pcm` as the bed instead of rendering it from the project (the
    /// project volume still applies through [`set_volume`](Self::set_volume)).
    /// It must be in [`format`](Self::format).
    pub fn set_pcm(&self, pcm: PcmAudio) -> Result<(), AudioError> {
        let format = AudioFormat {
            sample_rate: pcm.sample_rate,
            channels: pcm.channels,
        };
        if format != self.shared.format {
            return Err(AudioError::InvalidFormat(format));
        }
        self.swap_bed(pcm.samples);
        *lock(&self.bed_key) = None;
        Ok(())
    }

    fn swap_bed(&self, samples: Vec<f32>) {
        *lock(&self.shared.bed) = Arc::new(samples);
        self.shared.bed_gen.fetch_add(1, Ordering::SeqCst);
    }

    /// Run `f` on the output (for example to inspect a test device).
    pub fn with_output<R>(&self, f: impl FnOnce(&mut dyn AudioOutput) -> R) -> R {
        f(lock(&self.output).as_mut())
    }
}

fn lock<T: ?Sized>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
pub(super) mod tests;
