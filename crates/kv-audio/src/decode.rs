//! Decode a music file to mono `f32` samples (symphonia) and resample to the
//! analysis rate (rubato).

use std::fs::File;
use std::path::Path;

use rubato::audioadapter_buffers::direct::InterleavedSlice;
use rubato::{Fft, FixedSync, Resampler};
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

/// Decode the first audio track of `path` and mix it down to mono.
pub(crate) fn decode_mono(path: &Path) -> Result<MonoAudio, Error> {
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
    let mut mono = Vec::new();
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
        interleaved.clear();
        buf.copy_to_vec_interleaved(&mut interleaved);
        let gain = 1.0 / channels as f32;
        mono.extend(
            interleaved
                .chunks_exact(channels)
                .map(|frame| frame.iter().sum::<f32>() * gain),
        );
    }

    let sample_rate = sample_rate.ok_or(Error::Unsupported("unknown sample rate"))?;
    Ok(MonoAudio {
        samples: mono,
        sample_rate,
    })
}

/// Resample mono audio from `from` Hz to `to` Hz. Returns the input unchanged
/// when the rates already match.
pub(crate) fn resample(samples: &[f32], from: u32, to: u32) -> Result<Vec<f32>, Error> {
    if from == to || samples.is_empty() {
        return Ok(samples.to_vec());
    }
    let mut resampler = Fft::<f32>::new(from as usize, to as usize, 1024, 1, FixedSync::Input)
        .map_err(|e| Error::Resample(e.to_string()))?;
    let input = InterleavedSlice::new(samples, 1, samples.len())
        .map_err(|e| Error::Resample(e.to_string()))?;
    let mut out = vec![0.0f32; resampler.process_all_needed_output_len(samples.len())];
    let capacity = out.len();
    let mut output = InterleavedSlice::new_mut(&mut out, 1, capacity)
        .map_err(|e| Error::Resample(e.to_string()))?;
    let (_, written) = resampler
        .process_all_into_buffer(&input, &mut output, samples.len(), None)
        .map_err(|e| Error::Resample(e.to_string()))?;
    out.truncate(written);
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
