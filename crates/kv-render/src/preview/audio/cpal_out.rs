//! [`AudioOutput`] on the default output device, via `cpal`.

use std::sync::mpsc;
use std::thread::JoinHandle;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, OutputCallbackInfo, SampleFormat, SizedSample, StreamConfig};
use kv_core::Micros;

use super::{AudioError, AudioFormat, AudioOutput, AudioRenderer};

type Reply = mpsc::Sender<Result<(), AudioError>>;

/// The system's default output device, at its default rate. The bed is
/// rendered in stereo (mono for a one-channel device); extra device
/// channels get silence. The `cpal` stream lives on its own thread (streams
/// are not `Send` on every platform) and stops when this is dropped.
pub struct CpalOutput {
    format: AudioFormat,
    start: Option<mpsc::Sender<(AudioRenderer, Reply)>>,
    /// The started channel, held to keep the stream alive; dropped to stop.
    stop: Option<mpsc::Sender<(AudioRenderer, Reply)>>,
    thread: Option<JoinHandle<()>>,
}

impl CpalOutput {
    /// Open the default output device.
    pub fn open_default() -> Result<Self, AudioError> {
        let (opened_tx, opened_rx) = mpsc::channel();
        let (start_tx, start_rx) = mpsc::channel::<(AudioRenderer, Reply)>();
        let thread = std::thread::Builder::new()
            .name("kv-preview-audio".into())
            .spawn(move || {
                let device = match open() {
                    Ok((device, config, format, sample_format)) => {
                        let _ = opened_tx.send(Ok(format));
                        (device, config, sample_format)
                    }
                    Err(e) => {
                        let _ = opened_tx.send(Err(e));
                        return;
                    }
                };
                let (device, config, sample_format) = device;
                // Keep the stream alive until the output is dropped (the
                // channel closes).
                let mut _stream = None;
                for (renderer, reply) in start_rx {
                    match build(&device, config, sample_format, renderer) {
                        Ok(stream) => {
                            _stream = Some(stream);
                            let _ = reply.send(Ok(()));
                        }
                        Err(e) => {
                            let _ = reply.send(Err(e));
                        }
                    }
                }
            })
            .map_err(|e| AudioError::Device(e.to_string()))?;
        let format = opened_rx
            .recv()
            .map_err(|_| AudioError::Device("audio thread exited".into()))??;
        Ok(CpalOutput {
            format,
            start: Some(start_tx),
            stop: None,
            thread: Some(thread),
        })
    }
}

impl AudioOutput for CpalOutput {
    fn format(&self) -> AudioFormat {
        self.format
    }

    fn has_device_clock(&self) -> bool {
        true
    }

    fn start(&mut self, renderer: AudioRenderer) -> Result<(), AudioError> {
        // Taking the sender makes a second start fail; the thread keeps
        // the stream until the receiver sees the channel close.
        let start = self.start.take().ok_or(AudioError::AlreadyStarted)?;
        let (tx, rx) = mpsc::channel();
        start
            .send((renderer, tx))
            .map_err(|_| AudioError::Device("audio thread exited".into()))?;
        self.stop = Some(start);
        rx.recv()
            .map_err(|_| AudioError::Device("audio thread exited".into()))?
    }
}

impl Drop for CpalOutput {
    fn drop(&mut self) {
        self.start = None;
        self.stop = None;
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn open() -> Result<(cpal::Device, StreamConfig, AudioFormat, SampleFormat), AudioError> {
    let host = cpal::default_host();
    let device = host.default_output_device().ok_or(AudioError::NoDevice)?;
    let supported = device
        .default_output_config()
        .map_err(|e| AudioError::Device(e.to_string()))?;
    let config: StreamConfig = supported.into();
    let format = AudioFormat {
        sample_rate: config.sample_rate,
        channels: config.channels.clamp(1, 2),
    };
    if config.channels == 0 || format.sample_rate == 0 {
        return Err(AudioError::InvalidFormat(format));
    }
    Ok((device, config, format, supported.sample_format()))
}

fn build(
    device: &cpal::Device,
    config: StreamConfig,
    sample_format: SampleFormat,
    renderer: AudioRenderer,
) -> Result<cpal::Stream, AudioError> {
    let stream = match sample_format {
        SampleFormat::F32 => stream::<f32>(device, config, renderer),
        SampleFormat::F64 => stream::<f64>(device, config, renderer),
        SampleFormat::I16 => stream::<i16>(device, config, renderer),
        SampleFormat::I32 => stream::<i32>(device, config, renderer),
        SampleFormat::I8 => stream::<i8>(device, config, renderer),
        SampleFormat::U16 => stream::<u16>(device, config, renderer),
        SampleFormat::U8 => stream::<u8>(device, config, renderer),
        other => return Err(AudioError::Device(format!("sample format {other}"))),
    }
    .map_err(|e| AudioError::Device(e.to_string()))?;
    stream
        .play()
        .map_err(|e| AudioError::Device(e.to_string()))?;
    Ok(stream)
}

fn stream<T: SizedSample + FromSample<f32>>(
    device: &cpal::Device,
    config: StreamConfig,
    mut renderer: AudioRenderer,
) -> Result<cpal::Stream, cpal::Error> {
    let device_channels = usize::from(config.channels);
    let channels = usize::from(renderer.format().channels);
    let mut scratch: Vec<f32> = Vec::new();
    device.build_output_stream(
        config,
        move |data: &mut [T], info: &OutputCallbackInfo| {
            let ts = info.timestamp();
            let latency = ts.playback.duration_since(ts.callback);
            let frames = data.len() / device_channels;
            // Grows to the largest buffer once, then no allocation.
            scratch.resize(frames * channels, 0.0);
            renderer.render(
                &mut scratch,
                Micros(i64::try_from(latency.as_micros()).unwrap_or(i64::MAX)),
            );
            for (out, frame) in data
                .chunks_exact_mut(device_channels)
                .zip(scratch.chunks_exact(channels))
            {
                for (c, s) in out.iter_mut().enumerate() {
                    *s = T::from_sample(frame.get(c).copied().unwrap_or(0.0));
                }
            }
        },
        |e| tracing::warn!("preview audio: {e}"),
        None,
    )
}
