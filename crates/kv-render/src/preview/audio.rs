//! Preview audio: the music bed played through an output device, and a
//! [`Clock`] driven by the device so picture follows sound.
//!
//! - [`AudioTransport`] is the state shared between the engine and the
//!   device callback: the rendered bed, play/pause, the mapping from device
//!   frames to bed frames, and the live volume. The device pulls samples
//!   with [`AudioTransport::fill`].
//! - [`AudioClock`] (from [`AudioTransport::clock`]) is the time of the
//!   device frame being heard now: frames handed to the device, minus its
//!   reported output latency, interpolated with wall time between callbacks
//!   (at most one buffer) and never going backwards.
//! - Sync is exact by construction: on play or seek the engine passes the
//!   clock value it anchors the picture to, and the transport maps device
//!   frame `i` to bed frame `pos + (i - anchor)`, so the bed frame heard at
//!   clock `t` is the one at playhead `pos + (t - anchor)`, the same as the
//!   frame on screen. Audio already queued in the device when the user
//!   seeks is not re-rendered; at most one buffer (~10 ms) of it plays out.
//! - [`AudioOutput`] is the device: [`NullOutput`] for tests and headless
//!   hosts (pulled by hand), `CpalOutput` (cpal; macOS, or the
//!   `audio-device` feature elsewhere) for real playback. Without a device
//!   the engine keeps its [`SystemClock`](super::SystemClock).
//! - The bed is the music alone, or the music with the clip audio mixed
//!   under it (built by the engine, see `PreviewEngine::set_edit`).
//!   Swapping it ([`AudioTransport::set_bed_and_volume`]) keeps the play
//!   state and the frame mapping, so the clock does not jump and the next
//!   buffer continues from the same bed position.

use std::sync::atomic::{AtomicI64, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use kv_audio::PcmAudio;
use kv_core::Micros;

use super::Clock;

/// Errors from preview audio.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AudioError {
    #[error("no audio output device")]
    NoDevice,
    #[error("unsupported audio format: {0}")]
    Unsupported(String),
    #[error("audio device error: {0}")]
    Device(String),
    #[error("bed is {got_rate} Hz x{got_channels}, transport is {rate} Hz x{channels}")]
    FormatMismatch {
        rate: u32,
        channels: u16,
        got_rate: u32,
        got_channels: u16,
    },
}

/// An audio output device that pulls samples from an [`AudioTransport`].
/// Not `Send`: some backends' streams must stay on the thread that made
/// them; keep the output alive for as long as audio should play.
pub trait AudioOutput {
    /// Device sample rate in Hz.
    fn sample_rate(&self) -> u32;
    /// Channels the transport renders (1 or 2; extra device channels are
    /// left silent).
    fn channels(&self) -> u16;
    /// Start pulling from `transport`, whose format must match.
    fn start(&mut self, transport: Arc<AudioTransport>) -> Result<(), AudioError>;
}

/// Where the transport plays from.
struct Playback {
    playing: bool,
    /// While playing, device frame `i` plays bed frame `i + origin`.
    origin: i64,
    bed: Option<Arc<PcmAudio>>,
}

/// The last device callback, for the clock.
#[derive(Clone, Copy)]
struct Callback {
    /// Device frames handed over before this callback.
    before: i64,
    /// Frames in this callback.
    len: i64,
    /// Wall time when it ran.
    at: Micros,
    /// Output latency reported for it.
    latency: Micros,
}

/// Playback state shared between the preview engine and the device
/// callback. See the module docs.
pub struct AudioTransport {
    sample_rate: u32,
    channels: u16,
    playback: Mutex<Playback>,
    /// `f32` bits of the linear gain.
    volume: AtomicU32,
    /// Device frames handed over so far.
    written: AtomicU64,
    last_cb: Mutex<Option<Callback>>,
    /// Last value the clock returned (monotonic guard).
    heard: AtomicI64,
    wall: Arc<dyn Clock>,
}

impl AudioTransport {
    /// A paused transport with no bed, for a device at `sample_rate` Hz with
    /// `channels` (1 or 2) channels. `wall` interpolates the clock between
    /// callbacks (a [`SystemClock`](super::SystemClock) outside tests).
    pub fn new(sample_rate: u32, channels: u16, wall: Arc<dyn Clock>) -> Arc<Self> {
        Arc::new(AudioTransport {
            sample_rate: sample_rate.max(1),
            channels: channels.clamp(1, 2),
            playback: Mutex::new(Playback {
                playing: false,
                origin: 0,
                bed: None,
            }),
            volume: AtomicU32::new(1.0f32.to_bits()),
            written: AtomicU64::new(0),
            last_cb: Mutex::new(None),
            heard: AtomicI64::new(0),
            wall,
        })
    }

    /// Sample rate in Hz.
    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    /// Channels per frame.
    pub fn channels(&self) -> u16 {
        self.channels
    }

    /// Replace the bed (bed frame 0 = playhead 0), keeping the play state
    /// and position. `None` plays silence.
    pub fn set_bed(&self, bed: Option<Arc<PcmAudio>>) -> Result<(), AudioError> {
        self.swap_bed(bed, None)
    }

    /// [`set_bed`](Self::set_bed) and [`set_volume`](Self::set_volume) at
    /// once: no device buffer sees the new bed at the old gain or the other
    /// way round. The old bed is freed outside the lock the device takes.
    pub fn set_bed_and_volume(
        &self,
        bed: Option<Arc<PcmAudio>>,
        gain: f32,
    ) -> Result<(), AudioError> {
        self.swap_bed(bed, Some(gain))
    }

    fn swap_bed(&self, bed: Option<Arc<PcmAudio>>, gain: Option<f32>) -> Result<(), AudioError> {
        if let Some(b) = &bed {
            if b.sample_rate != self.sample_rate || b.channels != self.channels {
                return Err(AudioError::FormatMismatch {
                    rate: self.sample_rate,
                    channels: self.channels,
                    got_rate: b.sample_rate,
                    got_channels: b.channels,
                });
            }
        }
        let mut p = self.lock_playback();
        let old = std::mem::replace(&mut p.bed, bed);
        if let Some(g) = gain {
            self.set_volume(g);
        }
        drop(p);
        drop(old);
        Ok(())
    }

    /// Linear gain applied live (1.0 = the bed as rendered).
    pub fn set_volume(&self, gain: f32) {
        self.volume
            .store(gain.max(0.0).to_bits(), Ordering::Relaxed);
    }

    /// Current linear gain.
    pub fn volume(&self) -> f32 {
        f32::from_bits(self.volume.load(Ordering::Relaxed))
    }

    /// Play (or stay paused) so that the bed frame heard at clock time
    /// `anchor` is the one at playhead `pos`. Called by the engine on play,
    /// seek and edit with the clock value it anchored the picture to; takes
    /// effect from the next device callback.
    pub fn sync(&self, playing: bool, pos: Micros, anchor: Micros) {
        let mut p = self.lock_playback();
        p.playing = playing;
        p.origin = self.to_frames(pos) - self.to_frames(anchor);
    }

    /// Stop at once; the device plays silence.
    pub fn pause(&self) {
        self.lock_playback().playing = false;
    }

    /// True while playing.
    pub fn is_playing(&self) -> bool {
        self.lock_playback().playing
    }

    /// A clock reading the device's playback position.
    pub fn clock(self: &Arc<Self>) -> Arc<AudioClock> {
        Arc::new(AudioClock {
            transport: Arc::clone(self),
        })
    }

    /// Device side: write the next `out.len() / channels` interleaved
    /// frames. `latency` is the device's delay from this call to the first
    /// frame being heard (zero if unknown). Never blocks: if the engine
    /// holds the state this instant, the buffer is silence.
    pub fn fill(&self, out: &mut [f32], latency: Micros) {
        let ch = usize::from(self.channels);
        let n = out.len() / ch;
        let before = self.written.fetch_add(n as u64, Ordering::SeqCst) as i64;
        out.fill(0.0);
        if let Ok(p) = self.playback.try_lock() {
            if let (true, Some(bed)) = (p.playing, &p.bed) {
                let gain = self.volume();
                let frames = bed.frames() as i64;
                let first = before + p.origin;
                let lo = (-first).clamp(0, n as i64) as usize;
                let hi = (frames - first).clamp(0, n as i64) as usize;
                if lo < hi {
                    let src = &bed.samples[(first + lo as i64) as usize * ch..];
                    for (o, s) in out[lo * ch..hi * ch].iter_mut().zip(src) {
                        *o = (s * gain).clamp(-1.0, 1.0);
                    }
                }
            }
        }
        let cb = Callback {
            before,
            len: n as i64,
            at: self.wall.now(),
            latency: Micros(latency.0.max(0)),
        };
        if let Ok(mut last) = self.last_cb.try_lock() {
            *last = Some(cb);
        }
    }

    fn now(&self) -> Micros {
        let cb = *self.last_cb.lock().unwrap_or_else(PoisonError::into_inner);
        let Some(cb) = cb else {
            return Micros(self.heard.load(Ordering::SeqCst));
        };
        let elapsed = (self.wall.now().0 - cb.at.0).clamp(0, self.to_micros(cb.len).0);
        let t = self.to_micros(cb.before).0 - cb.latency.0 + elapsed;
        Micros(self.heard.fetch_max(t, Ordering::SeqCst).max(t))
    }

    fn to_frames(&self, t: Micros) -> i64 {
        let r = i128::from(self.sample_rate);
        ((i128::from(t.0) * r + 500_000).div_euclid(1_000_000)) as i64
    }

    fn to_micros(&self, frames: i64) -> Micros {
        Micros((i128::from(frames) * 1_000_000 / i128::from(self.sample_rate)) as i64)
    }

    fn lock_playback(&self) -> std::sync::MutexGuard<'_, Playback> {
        self.playback.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// The [`Clock`] of an [`AudioTransport`]: time of the device frame being
/// heard. Starts at 0 and stands still until the device first pulls.
pub struct AudioClock {
    transport: Arc<AudioTransport>,
}

impl Clock for AudioClock {
    fn now(&self) -> Micros {
        self.transport.now()
    }
}

/// An output with no device: nothing plays until [`NullOutput::pull`] is
/// called, which stands in for the device callback (tests, headless runs).
pub struct NullOutput {
    sample_rate: u32,
    channels: u16,
    transport: Option<Arc<AudioTransport>>,
}

impl NullOutput {
    /// A fake device at `sample_rate` Hz with `channels` channels.
    pub fn new(sample_rate: u32, channels: u16) -> Self {
        NullOutput {
            sample_rate,
            channels: channels.clamp(1, 2),
            transport: None,
        }
    }

    /// Pull `frames` frames as the device would, with output `latency`.
    /// Returns the interleaved samples (empty before `start`).
    pub fn pull(&self, frames: usize, latency: Micros) -> Vec<f32> {
        let Some(t) = &self.transport else {
            return Vec::new();
        };
        let mut buf = vec![0.0; frames * usize::from(self.channels)];
        t.fill(&mut buf, latency);
        buf
    }
}

impl AudioOutput for NullOutput {
    fn sample_rate(&self) -> u32 {
        self.sample_rate
    }
    fn channels(&self) -> u16 {
        self.channels
    }
    fn start(&mut self, transport: Arc<AudioTransport>) -> Result<(), AudioError> {
        check_format(self, &transport)?;
        self.transport = Some(transport);
        Ok(())
    }
}

fn check_format(out: &dyn AudioOutput, t: &AudioTransport) -> Result<(), AudioError> {
    if out.sample_rate() != t.sample_rate() || out.channels() != t.channels() {
        return Err(AudioError::FormatMismatch {
            rate: out.sample_rate(),
            channels: out.channels(),
            got_rate: t.sample_rate(),
            got_channels: t.channels(),
        });
    }
    Ok(())
}

/// Open the default output device and start it on a new transport. `None`
/// when there is no device, or this build has no device backend (Linux
/// without the `audio-device` feature): the preview then keeps its system
/// clock and plays no sound.
pub fn start_default_output() -> Option<(Box<dyn AudioOutput>, Arc<AudioTransport>)> {
    #[cfg(any(target_os = "macos", feature = "audio-device"))]
    {
        let mut out = match cpal_output::CpalOutput::default_device() {
            Ok(o) => o,
            Err(e) => {
                tracing::warn!("preview audio: {e}");
                return None;
            }
        };
        let transport = AudioTransport::new(
            out.sample_rate(),
            out.channels(),
            Arc::new(super::SystemClock::new()),
        );
        match out.start(Arc::clone(&transport)) {
            Ok(()) => Some((Box::new(out), transport)),
            Err(e) => {
                tracing::warn!("preview audio: {e}");
                None
            }
        }
    }
    #[cfg(not(any(target_os = "macos", feature = "audio-device")))]
    {
        None
    }
}

#[cfg(any(target_os = "macos", feature = "audio-device"))]
pub use cpal_output::CpalOutput;

#[cfg(any(target_os = "macos", feature = "audio-device"))]
mod cpal_output {
    use std::sync::Arc;

    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
    use kv_core::Micros;

    use super::{check_format, AudioError, AudioOutput, AudioTransport};

    /// The system's default output device through cpal (CoreAudio on
    /// macOS), streaming `f32`.
    pub struct CpalOutput {
        device: cpal::Device,
        config: cpal::StreamConfig,
        channels: u16,
        stream: Option<cpal::Stream>,
    }

    impl CpalOutput {
        /// The default output device at its default rate.
        pub fn default_device() -> Result<Self, AudioError> {
            let device = cpal::default_host()
                .default_output_device()
                .ok_or(AudioError::NoDevice)?;
            let supported = device
                .default_output_config()
                .map_err(|e| AudioError::Device(e.to_string()))?;
            if supported.sample_format() != cpal::SampleFormat::F32 {
                return Err(AudioError::Unsupported(format!(
                    "{:?} samples",
                    supported.sample_format()
                )));
            }
            let config = supported.config();
            if config.channels == 0 {
                return Err(AudioError::Unsupported("0 channels".into()));
            }
            Ok(CpalOutput {
                device,
                channels: config.channels.min(2),
                config,
                stream: None,
            })
        }
    }

    impl AudioOutput for CpalOutput {
        fn sample_rate(&self) -> u32 {
            self.config.sample_rate.0
        }

        fn channels(&self) -> u16 {
            self.channels
        }

        fn start(&mut self, transport: Arc<AudioTransport>) -> Result<(), AudioError> {
            check_format(self, &transport)?;
            let dev_ch = usize::from(self.config.channels);
            let ch = usize::from(self.channels);
            let mut scratch: Vec<f32> = Vec::new();
            let stream = self
                .device
                .build_output_stream(
                    &self.config,
                    move |data: &mut [f32], info: &cpal::OutputCallbackInfo| {
                        let ts = info.timestamp();
                        let latency = ts
                            .playback
                            .duration_since(&ts.callback)
                            .map_or(Micros(0), |d| {
                                Micros(i64::try_from(d.as_micros()).unwrap_or(0))
                            });
                        if dev_ch == ch {
                            transport.fill(data, latency);
                            return;
                        }
                        // More device channels than we render: fill the
                        // first one or two, leave the rest silent.
                        let frames = data.len() / dev_ch;
                        scratch.resize(frames * ch, 0.0);
                        transport.fill(&mut scratch, latency);
                        for (o, s) in data.chunks_mut(dev_ch).zip(scratch.chunks(ch)) {
                            o.fill(0.0);
                            o[..ch].copy_from_slice(s);
                        }
                    },
                    |e| tracing::warn!("preview audio stream: {e}"),
                    None,
                )
                .map_err(|e| AudioError::Device(e.to_string()))?;
            stream
                .play()
                .map_err(|e| AudioError::Device(e.to_string()))?;
            self.stream = Some(stream);
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::ManualClock;
    use super::*;

    const RATE: u32 = 1_000; // 1 frame = 1 ms keeps the arithmetic readable

    /// A mono bed whose sample `i` is `i / 10_000`.
    fn ramp(frames: usize) -> Arc<PcmAudio> {
        Arc::new(PcmAudio {
            samples: (0..frames).map(|i| i as f32 / 10_000.0).collect(),
            sample_rate: RATE,
            channels: 1,
        })
    }

    fn rig() -> (Arc<ManualClock>, Arc<AudioTransport>, NullOutput) {
        let wall = Arc::new(ManualClock::new());
        let t = AudioTransport::new(RATE, 1, wall.clone());
        let mut out = NullOutput::new(RATE, 1);
        out.start(Arc::clone(&t)).unwrap();
        (wall, t, out)
    }

    fn index(s: f32) -> i64 {
        (s * 10_000.0).round() as i64
    }

    #[test]
    fn paused_is_silent_and_play_starts_at_the_playhead() {
        let (_, t, out) = rig();
        t.set_bed(Some(ramp(5_000))).unwrap();
        assert!(out.pull(10, Micros(0)).iter().all(|s| *s == 0.0));
        // Device has handed over 10 frames; anchor the playhead 2 s at
        // clock 10 ms: device frame 10 plays bed frame 2000.
        t.sync(true, Micros(2_000_000), Micros(10_000));
        let got: Vec<i64> = out.pull(5, Micros(0)).into_iter().map(index).collect();
        assert_eq!(got, vec![2_000, 2_001, 2_002, 2_003, 2_004]);
        t.pause();
        assert!(out.pull(5, Micros(0)).iter().all(|s| *s == 0.0));
    }

    #[test]
    fn seek_repositions_on_the_next_buffer() {
        let (_, t, out) = rig();
        t.set_bed(Some(ramp(5_000))).unwrap();
        t.sync(true, Micros(0), Micros(0));
        assert_eq!(index(out.pull(10, Micros(0))[0]), 0);
        // Seek to 3 s at clock 10 ms (frames 0..10 heard so far).
        t.sync(true, Micros(3_000_000), Micros(10_000));
        let got = out.pull(10, Micros(0));
        assert_eq!(index(got[0]), 3_000);
        assert_eq!(index(got[9]), 3_009);
    }

    #[test]
    fn volume_applies_live_and_past_the_bed_is_silence() {
        let (_, t, out) = rig();
        t.set_bed(Some(ramp(20))).unwrap();
        t.sync(true, Micros(10_000), Micros(0));
        t.set_volume(0.5);
        let got = out.pull(15, Micros(0));
        assert!((got[0] - 0.0005).abs() < 1e-6, "{}", got[0]);
        assert!(got[10..].iter().all(|s| *s == 0.0));
        assert_eq!(t.volume(), 0.5);
    }

    #[test]
    fn swapping_the_bed_while_playing_keeps_position_and_clock() {
        let (wall, t, out) = rig();
        let clock = t.clock();
        t.set_bed(Some(ramp(5_000))).unwrap();
        t.sync(true, Micros(1_000_000), Micros(0));
        assert_eq!(index(out.pull(10, Micros(0))[9]), 1_009);
        wall.advance(Micros(10_000));
        let before = clock.now();
        // A louder copy of the bed at a new gain: the next buffer carries on
        // from bed frame 1010 at the new level, the clock is unchanged.
        let louder = Arc::new(PcmAudio {
            samples: ramp(5_000).samples.iter().map(|s| s * 2.0).collect(),
            sample_rate: RATE,
            channels: 1,
        });
        t.set_bed_and_volume(Some(louder), 0.5).unwrap();
        assert_eq!(clock.now(), before);
        assert!(t.is_playing());
        assert_eq!(t.volume(), 0.5);
        let got = out.pull(5, Micros(0));
        assert_eq!(
            got.iter().map(|s| index(*s)).collect::<Vec<_>>(),
            [1_010, 1_011, 1_012, 1_013, 1_014]
        );
    }

    #[test]
    fn rejects_a_bed_in_another_format() {
        let (_, t, _) = rig();
        let bed = Arc::new(PcmAudio {
            samples: vec![0.0; 4],
            sample_rate: 48_000,
            channels: 2,
        });
        assert!(matches!(
            t.set_bed(Some(bed)),
            Err(AudioError::FormatMismatch { .. })
        ));
        let mut out = NullOutput::new(48_000, 2);
        assert!(out.start(t).is_err());
    }

    #[test]
    fn clock_follows_frames_played_minus_latency() {
        let (wall, t, out) = rig();
        let clock = t.clock();
        // Stands still until the device pulls.
        wall.set(Micros(500_000));
        assert_eq!(clock.now(), Micros(0));
        // 10 ms buffers with 20 ms latency.
        out.pull(10, Micros(20_000));
        out.pull(10, Micros(20_000));
        out.pull(10, Micros(20_000));
        // Last callback started at device frame 20 (20 ms), heard 20 ms
        // later: the clock reads 0, then interpolates within the buffer.
        assert_eq!(clock.now(), Micros(0));
        wall.advance(Micros(4_000));
        assert_eq!(clock.now(), Micros(4_000));
        // Interpolation stops at the buffer's end.
        wall.advance(Micros(50_000));
        assert_eq!(clock.now(), Micros(10_000));
        // A callback that would move the clock back cannot.
        out.pull(10, Micros(40_000));
        assert_eq!(clock.now(), Micros(10_000));
        out.pull(10, Micros(0));
        assert_eq!(clock.now(), Micros(40_000));
    }
}
