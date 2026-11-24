//! Headless tests: a fake device that the test pulls buffers from by hand,
//! and a [`ManualClock`] as the system-time reference.

use super::*;
use crate::preview::ManualClock;

/// 1 kHz mono: one frame per millisecond, so values and times line up.
pub(crate) const FMT: AudioFormat = AudioFormat {
    sample_rate: 1_000,
    channels: 1,
};

/// A device driven by the test: [`DeviceHandle::pull`] plays one buffer.
pub(crate) struct FakeDevice {
    format: AudioFormat,
    renderer: Arc<Mutex<Option<AudioRenderer>>>,
}

/// The test's side of a [`FakeDevice`].
#[derive(Clone)]
pub(crate) struct DeviceHandle {
    renderer: Arc<Mutex<Option<AudioRenderer>>>,
    reference: Arc<ManualClock>,
    format: AudioFormat,
}

impl DeviceHandle {
    /// Play `frames` frames reported `latency` ahead, as a real device
    /// would: render the buffer, then let its duration pass.
    pub(crate) fn pull(&self, frames: usize, latency: Micros) -> Vec<f32> {
        let mut out = vec![9.0; frames * usize::from(self.format.channels)];
        lock(&self.renderer)
            .as_mut()
            .expect("started")
            .render(&mut out, latency);
        self.reference
            .advance(Micros(micros_at(frames as u64, self.format.sample_rate)));
        out
    }
}

impl AudioOutput for FakeDevice {
    fn format(&self) -> AudioFormat {
        self.format
    }
    fn has_device_clock(&self) -> bool {
        true
    }
    fn start(&mut self, renderer: AudioRenderer) -> Result<(), AudioError> {
        *lock(&self.renderer) = Some(renderer);
        Ok(())
    }
}

/// Preview audio on a fake device in `format`, and the handle to pull it.
pub(crate) fn fake(format: AudioFormat) -> (PreviewAudio, DeviceHandle) {
    let renderer = Arc::new(Mutex::new(None));
    let reference = Arc::new(ManualClock::new());
    let device = FakeDevice {
        format,
        renderer: Arc::clone(&renderer),
    };
    let audio = PreviewAudio::with_reference(Box::new(device), reference.clone()).unwrap();
    (
        audio,
        DeviceHandle {
            renderer,
            reference,
            format,
        },
    )
}

/// A bed whose frame `f` has value `(f + 1) / 10_000`, so a sample tells
/// which bed frame (timeline millisecond at [`FMT`]) it came from.
pub(crate) fn ramp(frames: usize) -> PcmAudio {
    PcmAudio {
        samples: (0..frames).map(|f| (f + 1) as f32 / 10_000.0).collect(),
        sample_rate: FMT.sample_rate,
        channels: FMT.channels,
    }
}

/// The bed frames `samples` came from (`None` for silence).
pub(crate) fn frames_of(samples: &[f32]) -> Vec<Option<i64>> {
    samples
        .iter()
        .map(|&s| (s != 0.0).then(|| (s * 10_000.0).round() as i64 - 1))
        .collect()
}

fn playing(pos: i64, anchor: i64, end: i64) -> Transport {
    Transport {
        playing: true,
        pos: Micros(pos),
        anchor: Micros(anchor),
        end: Micros(end),
    }
}

fn run(from: i64, n: i64) -> Vec<Option<i64>> {
    (from..from + n).map(Some).collect()
}

#[test]
fn clock_counts_frames_played_minus_latency() {
    let (audio, dev) = fake(AudioFormat::DEFAULT);
    let clock = audio.clock();
    assert!(clock.is_device() && audio.has_device());
    assert_eq!(clock.now(), Micros(0));
    // 10 ms buffers at 48 kHz, no latency: the clock is at the end of what
    // has been played.
    dev.pull(480, Micros(0));
    assert_eq!(clock.now(), Micros(10_000));
    dev.pull(480, Micros(0));
    assert_eq!(clock.now(), Micros(20_000));
    // The latency grows to 20 ms: the clock would step back 20 ms, so it
    // holds until the device catches up, never going backwards.
    dev.pull(480, Micros(20_000));
    assert_eq!(clock.now(), Micros(20_000));
    dev.pull(480, Micros(20_000));
    assert_eq!(clock.now(), Micros(20_000));
    dev.pull(480, Micros(20_000));
    assert_eq!(clock.now(), Micros(30_000));

    // A constant 30 ms latency from the start: after 50 ms written, 20 ms
    // have been heard.
    let (audio, dev) = fake(AudioFormat::DEFAULT);
    for _ in 0..5 {
        dev.pull(480, Micros(30_000));
    }
    assert_eq!(audio.clock().now(), Micros(20_000));
}

#[test]
fn clock_interpolates_between_callbacks_up_to_one_buffer() {
    let (audio, dev) = fake(AudioFormat::DEFAULT);
    let clock = audio.clock();
    dev.pull(480, Micros(0)); // reference now at 10 ms, clock 10 ms
    dev.reference.advance(Micros(4_000));
    // Still the same buffer: system time fills in, capped at its length.
    assert_eq!(clock.now(), Micros(10_000));
    let mut out = vec![0.0; 960]; // 480 stereo frames: 10 ms
    lock(&dev.renderer)
        .as_mut()
        .unwrap()
        .render(&mut out, Micros(0));
    // Callback at 14 ms reference for frames from 10 ms.
    assert_eq!(clock.now(), Micros(10_000));
    dev.reference.advance(Micros(3_000));
    assert_eq!(clock.now(), Micros(13_000));
    dev.reference.advance(Micros(50_000));
    assert_eq!(clock.now(), Micros(20_000), "capped at the buffer's end");
}

#[test]
fn null_output_falls_back_to_system_time() {
    let reference = Arc::new(ManualClock::new());
    let audio =
        PreviewAudio::with_reference(Box::new(NullOutput::new()), reference.clone()).unwrap();
    let clock = audio.clock();
    assert!(!clock.is_device() && !audio.has_device());
    assert_eq!(audio.format(), AudioFormat::DEFAULT);
    reference.set(Micros(123_456));
    assert_eq!(clock.now(), Micros(123_456));

    let a = audio.clock().now();
    assert!(audio.clock().now() >= a);
    // A null output starts once.
    let renderer = || AudioRenderer {
        shared: Arc::clone(&audio.shared),
        clock: None,
        bed: Arc::new(Vec::new()),
        bed_gen: 0,
        written: 0,
    };
    let mut null = NullOutput::default();
    assert!(null.start(renderer()).is_ok());
    assert!(matches!(
        null.start(renderer()),
        Err(AudioError::AlreadyStarted)
    ));
}

#[test]
fn plays_the_bed_from_the_playhead_and_stops_at_the_end() {
    let (audio, dev) = fake(FMT);
    audio.set_pcm(ramp(3_000)).unwrap();
    // Paused: silence.
    assert_eq!(frames_of(&dev.pull(100, Micros(0))), vec![None; 100]);
    // Play from 1 s at clock 100 ms: the bed from frame 1000 on.
    audio.set_transport(playing(1_000_000, 100_000, 2_000_000));
    assert_eq!(frames_of(&dev.pull(100, Micros(0))), run(1_000, 100));
    assert_eq!(frames_of(&dev.pull(50, Micros(0))), run(1_100, 50));
    // Position follows the clock exactly: play started mid-buffer.
    audio.set_transport(playing(500_000, 270_000, 2_000_000));
    let got = frames_of(&dev.pull(40, Micros(0))); // clock 250..290 ms
    let mut want = vec![None; 20];
    want.extend(run(500, 20));
    assert_eq!(got, want);
    // Past the edit end: silence.
    audio.set_transport(playing(1_990_000, 290_000, 2_000_000));
    let got = frames_of(&dev.pull(20, Micros(0)));
    let mut want = run(1_990, 10);
    want.extend(vec![None; 10]);
    assert_eq!(got, want);
    // Pause: silence again.
    audio.set_transport(Transport {
        playing: false,
        ..playing(1_995_000, 310_000, 2_000_000)
    });
    assert_eq!(frames_of(&dev.pull(10, Micros(0))), vec![None; 10]);
}

#[test]
fn latency_keeps_sound_in_step_with_the_clock() {
    // The sample written now is heard `latency` later, when the engine shows
    // the position `latency` further on: that is the sample written.
    let (audio, dev) = fake(FMT);
    audio.set_pcm(ramp(3_000)).unwrap();
    dev.pull(100, Micros(40_000));
    dev.pull(100, Micros(40_000)); // clock: 200 ms written - 40 ms latency
    let clock = audio.clock();
    let now = clock.now();
    assert_eq!(now, Micros(160_000));
    audio.set_transport(playing(0, now.0, 3_000_000));
    // Frames written from 200 ms are heard at clock 200 ms: position 40 ms.
    assert_eq!(frames_of(&dev.pull(10, Micros(40_000))), run(40, 10));
}

#[test]
fn seek_repositions_on_the_next_buffer() {
    let (audio, dev) = fake(FMT);
    audio.set_pcm(ramp(3_000)).unwrap();
    audio.set_transport(playing(0, 0, 3_000_000));
    // 10 ms device buffers: a seek is heard within one buffer (< 50 ms).
    assert_eq!(frames_of(&dev.pull(10, Micros(0))), run(0, 10));
    let now = audio.clock().now();
    audio.set_transport(playing(2_000_000, now.0, 3_000_000));
    assert_eq!(frames_of(&dev.pull(10, Micros(0))), run(2_000, 10));
    let now = audio.clock().now();
    audio.set_transport(playing(500_000, now.0, 3_000_000));
    assert_eq!(frames_of(&dev.pull(10, Micros(0))), run(500, 10));
}

#[test]
fn volume_changes_apply_on_the_next_buffer() {
    let (audio, dev) = fake(FMT);
    audio
        .set_pcm(PcmAudio {
            samples: vec![0.8; 1_000],
            sample_rate: 1_000,
            channels: 1,
        })
        .unwrap();
    audio.set_transport(playing(0, 0, 1_000_000));
    assert!(dev.pull(10, Micros(0)).iter().all(|&s| s == 0.8));
    audio.set_volume(50);
    assert!(dev.pull(10, Micros(0)).iter().all(|&s| s == 0.4));
    audio.set_volume(0);
    assert!(dev.pull(10, Micros(0)).iter().all(|&s| s == 0.0));
}

#[test]
fn stereo_frames_stay_interleaved() {
    let fmt = AudioFormat {
        sample_rate: 1_000,
        channels: 2,
    };
    let (audio, dev) = fake(fmt);
    let samples = (0..100).flat_map(|f| [f as f32 / 1000.0, -0.5]).collect();
    audio
        .set_pcm(PcmAudio {
            samples,
            sample_rate: 1_000,
            channels: 2,
        })
        .unwrap();
    audio.set_transport(playing(20_000, 0, 100_000));
    let out = dev.pull(3, Micros(0));
    assert_eq!(out, vec![0.02, -0.5, 0.021, -0.5, 0.022, -0.5]);
    // Wrong format is refused.
    assert!(matches!(
        audio.set_pcm(ramp(10)),
        Err(AudioError::InvalidFormat(_))
    ));
}

#[test]
fn invalid_device_format_is_rejected() {
    for format in [
        AudioFormat {
            sample_rate: 0,
            channels: 2,
        },
        AudioFormat {
            sample_rate: 48_000,
            channels: 6,
        },
    ] {
        let device = FakeDevice {
            format,
            renderer: Arc::new(Mutex::new(None)),
        };
        assert!(matches!(
            PreviewAudio::new(Box::new(device)),
            Err(AudioError::InvalidFormat(_))
        ));
    }
}

#[test]
fn set_section_renders_the_music_like_export() {
    use kv_core::{MediaId, MusicRef, ProjectId, Segment, Speed, Transition};

    let music = kv_testkit::click_track(120.0, 3.0, 8_000);
    let fmt = AudioFormat {
        sample_rate: 8_000,
        channels: 1,
    };
    let (audio, dev) = fake(fmt);
    let mut project = Project::new(ProjectId(1));
    let sec = Project::DEFAULT_SECTION;
    let seg = Segment {
        media_id: MediaId(1),
        src_in: Micros(0),
        src_out: Micros(2_000_000),
        timeline_start: Micros(0),
        transition: Transition::Cut,
        speed: Speed::NORMAL,
    };
    project.sections[0].edit.segments.push(seg);
    project.sections[0].music = Some(MusicRef {
        path: music.file_name().unwrap().to_str().unwrap().into(),
        duration: Micros(3_000_000),
    });
    project.settings.music_volume_percent = 60;
    let root = music.parent().unwrap();
    audio.set_section(&project, sec, root).unwrap();
    audio.set_transport(playing(0, 0, 2_000_000));
    let got = dev.pull(16_100, Micros(0));

    // Export renders the bed with the volume and fade baked in.
    let mut opts = MusicBedOptions::new(8_000, 1, Micros(2_000_000));
    opts.volume_percent = 60;
    opts.fade_out = crate::MUSIC_FADE_OUT;
    let want = render_music_bed(&music, &opts).unwrap().samples;
    assert_eq!(want.len(), 16_000);
    assert!(
        want.iter().any(|&s| s.abs() > 0.1),
        "click track is audible"
    );
    for (i, (a, b)) in got.iter().zip(&want).enumerate() {
        assert!((a - b).abs() < 1e-6, "sample {i}: {a} vs {b}");
    }
    assert!(
        got[16_000..].iter().all(|&s| s == 0.0),
        "silence after the end"
    );

    // Same music and length: no re-render, just the new volume.
    let gen = audio.shared.bed_gen.load(Ordering::SeqCst);
    project.settings.music_volume_percent = 100;
    audio.set_section(&project, sec, root).unwrap();
    assert_eq!(audio.shared.bed_gen.load(Ordering::SeqCst), gen);
    // Without music: silence.
    project.sections[0].music = None;
    audio.set_section(&project, sec, root).unwrap();
    assert_ne!(audio.shared.bed_gen.load(Ordering::SeqCst), gen);
    let now = audio.clock().now();
    audio.set_transport(playing(0, now.0, 2_000_000));
    assert!(dev.pull(100, Micros(0)).iter().all(|&s| s == 0.0));
    // Unknown section and unreadable music are errors.
    assert!(matches!(
        audio.set_section(&project, SectionId(99), root),
        Err(AudioError::UnknownSection(_))
    ));
    project.sections[0].music = Some(MusicRef {
        path: "missing.wav".into(),
        duration: Micros(1),
    });
    assert!(matches!(
        audio.set_section(&project, sec, root),
        Err(AudioError::Music { .. })
    ));
}

#[test]
fn transport_cell_round_trips() {
    let cell = TransportCell::default();
    assert_eq!(cell.load(), Transport::default());
    let t = playing(1, 2, 3);
    cell.store(t);
    assert_eq!(cell.load(), t);
}

#[test]
fn frame_math() {
    assert_eq!(frames_at(Micros(1_000_000), 48_000), 48_000);
    assert_eq!(frames_at(Micros(-5), 48_000), 0);
    assert_eq!(frames_signed(Micros(-1_000), 1_000), -1);
    assert_eq!(frames_signed(Micros(1_500), 1_000), 2);
    assert_eq!(micros_at(480, 48_000), 10_000);
}

#[test]
fn open_default_never_fails() {
    // CI has no audio device (or, without device support, no `cpal`): this
    // must fall back to a silent output on system time, not panic.
    let audio = PreviewAudio::open_default();
    let a = audio.clock().now();
    assert!(audio.clock().now() >= a);
    assert!((1..=2).contains(&audio.format().channels));
    audio.set_volume(50);
}
