//! Decode a music file to interleaved `f32` samples (symphonia), mix it down
//! to mono for analysis, and resample (rubato).

use std::fs::File;
use std::path::Path;

use rubato::audioadapter_buffers::direct::InterleavedSlice;
use rubato::{Fft, FixedSync, Resampler};
use symphonia::core::audio::{Channels, Position};
use symphonia::core::codecs::audio::AudioDecoderOptions;
use symphonia::core::errors::Error as SymError;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, TrackType};
use symphonia::core::io::{MediaSourceStream, MediaSourceStreamOptions};
use symphonia::core::meta::MetadataOptions;

use crate::Error;

/// Decoded mono audio.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct MonoAudio {
    pub samples: Vec<f32>,
    pub sample_rate: u32,
}

/// Which side of a stereo downmix a source channel feeds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Side {
    Left,
    Right,
    /// Fed equally to both sides (centre, mono, top/rear centre, ...).
    Both,
    /// Dropped from the downmix (low-frequency effects).
    Neither,
}

/// Decoded audio with its original channels, interleaved.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct DecodedAudio {
    /// Interleaved samples, `channels` per frame.
    pub samples: Vec<f32>,
    pub sample_rate: u32,
    /// Number of channels, at least 1.
    pub channels: usize,
    /// Downmix side of each channel, in buffer order; `channels` long.
    pub sides: Vec<Side>,
}

impl DecodedAudio {
    /// Mix down to mono: the plain average of all channels per frame.
    pub fn into_mono(self) -> MonoAudio {
        let samples = if self.channels == 1 {
            self.samples
        } else {
            let gain = 1.0 / self.channels as f32;
            self.samples
                .chunks_exact(self.channels)
                .map(|frame| frame.iter().sum::<f32>() * gain)
                .collect()
        };
        MonoAudio {
            samples,
            sample_rate: self.sample_rate,
        }
    }
}

/// Decode the first audio track of `path` and mix it down to mono.
pub(crate) fn decode_mono(path: &Path) -> Result<MonoAudio, Error> {
    Ok(decode_interleaved(path)?.into_mono())
}

/// Decode the first audio track of `path` to interleaved `f32` samples with
/// its original sample rate and channels.
pub(crate) fn decode_interleaved(path: &Path) -> Result<DecodedAudio, Error> {
    let file = File::open(path)?;
    let mss = MediaSourceStream::new(Box::new(file), MediaSourceStreamOptions::default());
    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }
    let mut format = symphonia::default::get_probe().probe(
        &hint,
        mss,
        FormatOptions::default(),
        MetadataOptions::default(),
    )?;

    let track = format
        .default_track(TrackType::Audio)
        .ok_or(Error::NoAudioTrack)?;
    let track_id = track.id;
    let params = track
        .codec_params
        .as_ref()
        .and_then(|p| p.audio())
        .ok_or(Error::NoAudioTrack)?
        .clone();
    let mut decoder = symphonia::default::get_codecs()
        .make_audio_decoder(&params, &AudioDecoderOptions::default())?;

    let mut sample_rate = params.sample_rate;
    let mut layout: Option<(usize, Vec<Side>)> = None;
    let mut samples = Vec::new();
    let mut interleaved: Vec<f32> = Vec::new();
    while let Some(packet) = format.next_packet()? {
        if packet.track_id != track_id {
            continue;
        }
        let buf = match decoder.decode(&packet) {
            Ok(buf) => buf,
            // A corrupt packet is skipped; decoding continues with the next.
            Err(SymError::DecodeError(_)) | Err(SymError::IoError(_)) => continue,
            Err(e) => return Err(e.into()),
        };
        let spec = buf.spec();
        let channels = spec.channels().count().max(1);
        sample_rate.get_or_insert(spec.rate());
        match &layout {
            None => layout = Some((channels, channel_sides(spec.channels(), channels))),
            Some((n, _)) if *n != channels => {
                return Err(Error::Unsupported("channel count changes mid-stream"))
            }
            Some(_) => {}
        }
        interleaved.clear();
        buf.copy_to_vec_interleaved(&mut interleaved);
        samples.extend_from_slice(&interleaved);
    }

    let sample_rate = sample_rate.ok_or(Error::Unsupported("unknown sample rate"))?;
    let (channels, sides) = layout.unwrap_or_else(|| (1, vec![Side::Both]));
    Ok(DecodedAudio {
        samples,
        sample_rate,
        channels,
        sides,
    })
}

/// Downmix side of each of the `count` channels of `layout`, in buffer order.
///
/// Positioned layouts use the speaker positions (symphonia orders the buffer
/// by ascending position bit): every `*LEFT*` position feeds the left side,
/// every `*RIGHT*` position the right side, LFE is dropped, and every other
/// (centre) position feeds both. Unpositioned layouts (discrete, ambisonic,
/// custom) alternate left, right by index; a single channel feeds both.
pub(crate) fn channel_sides(layout: &Channels, count: usize) -> Vec<Side> {
    const LEFT: Position = Position::FRONT_LEFT
        .union(Position::REAR_LEFT)
        .union(Position::FRONT_LEFT_CENTER)
        .union(Position::SIDE_LEFT)
        .union(Position::TOP_FRONT_LEFT)
        .union(Position::TOP_REAR_LEFT)
        .union(Position::TOP_SIDE_LEFT)
        .union(Position::BOTTOM_FRONT_LEFT)
        .union(Position::FRONT_LEFT_WIDE);
    const RIGHT: Position = Position::FRONT_RIGHT
        .union(Position::REAR_RIGHT)
        .union(Position::FRONT_RIGHT_CENTER)
        .union(Position::SIDE_RIGHT)
        .union(Position::TOP_FRONT_RIGHT)
        .union(Position::TOP_REAR_RIGHT)
        .union(Position::TOP_SIDE_RIGHT)
        .union(Position::BOTTOM_FRONT_RIGHT)
        .union(Position::FRONT_RIGHT_WIDE);
    const LFE: Position = Position::LFE1.union(Position::LFE2);

    if count == 1 {
        return vec![Side::Both];
    }
    match layout {
        Channels::Positioned(positions) if positions.bits().count_ones() as usize == count => (0
            ..64)
            .map(|bit| 1u64 << bit)
            .filter(|b| positions.bits() & b != 0)
            .map(|b| {
                let p = Position::from_bits_retain(b);
                if LEFT.contains(p) {
                    Side::Left
                } else if RIGHT.contains(p) {
                    Side::Right
                } else if LFE.contains(p) {
                    Side::Neither
                } else {
                    Side::Both
                }
            })
            .collect(),
        _ => (0..count)
            .map(|i| if i % 2 == 0 { Side::Left } else { Side::Right })
            .collect(),
    }
}

/// Resample mono audio from `from` Hz to `to` Hz. Returns the input unchanged
/// when the rates already match.
pub(crate) fn resample(samples: &[f32], from: u32, to: u32) -> Result<Vec<f32>, Error> {
    resample_interleaved(samples, 1, from, to)
}

/// Resample interleaved audio with `channels` channels from `from` Hz to `to`
/// Hz. Returns the input unchanged when the rates already match.
pub(crate) fn resample_interleaved(
    samples: &[f32],
    channels: usize,
    from: u32,
    to: u32,
) -> Result<Vec<f32>, Error> {
    if from == to || samples.is_empty() {
        return Ok(samples.to_vec());
    }
    let frames = samples.len() / channels;
    let mut resampler =
        Fft::<f32>::new(from as usize, to as usize, 1024, channels, FixedSync::Input)
            .map_err(|e| Error::Resample(e.to_string()))?;
    let input = InterleavedSlice::new(samples, channels, frames)
        .map_err(|e| Error::Resample(e.to_string()))?;
    let capacity = resampler.process_all_needed_output_len(frames);
    let mut out = vec![0.0f32; capacity * channels];
    let mut output = InterleavedSlice::new_mut(&mut out, channels, capacity)
        .map_err(|e| Error::Resample(e.to_string()))?;
    let (_, written) = resampler
        .process_all_into_buffer(&input, &mut output, frames, None)
        .map_err(|e| Error::Resample(e.to_string()))?;
    out.truncate(written * channels);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resample_halves_length_and_keeps_timing() {
        // One impulse at 1.0 s in 2 s of 44.1 kHz audio.
        let mut x = vec![0.0f32; 88_200];
        x[44_100] = 1.0;
        let y = resample(&x, 44_100, 22_050).unwrap();
        assert!((y.len() as i64 - 44_100).abs() <= 1, "len {}", y.len());
        let peak = (0..y.len())
            .max_by(|&a, &b| y[a].abs().total_cmp(&y[b].abs()))
            .unwrap();
        assert!((peak as i64 - 22_050).abs() <= 2, "peak at {peak}");
    }

    #[test]
    fn resample_same_rate_is_identity() {
        let x = vec![0.1, -0.2, 0.3];
        assert_eq!(resample(&x, 22_050, 22_050).unwrap(), x);
    }
}
