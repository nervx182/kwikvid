//! Render the music bed for export: the song as interleaved `f32` PCM at the
//! encoder's rate and channel count, trimmed to the edit's length, with the
//! project's music volume and fades applied.

use std::path::Path;

use kv_core::Micros;

use crate::decode::{self, DecodedAudio, Side};
use crate::Error;

/// Default [`MusicBedOptions::fade_out`]: 1 s (guess).
pub const DEFAULT_FADE_OUT: Micros = Micros(1_000_000);

/// Frames of extra source audio kept on each side of the trimmed range when
/// resampling, so the resampler's filter sees real context at the edges.
pub(crate) const RESAMPLE_MARGIN: usize = 2_048;

/// How to render the music bed. Build with [`MusicBedOptions::new`] and
/// override fields as needed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MusicBedOptions {
    /// Output sample rate in Hz (the encoder's, typically 48 000). Must be > 0.
    pub sample_rate: u32,
    /// Output channels: 1 (mono) or 2 (stereo).
    pub channels: u16,
    /// Output length; the output has exactly `round(length * sample_rate)`
    /// frames. Must be >= 0.
    pub length: Micros,
    /// Offset into the song where the bed starts. Must be >= 0. Default 0.
    pub start: Micros,
    /// Linear gain in percent (`volume_percent / 100`); values above 100
    /// amplify, and the result is clamped to [-1, 1]. Default 100.
    pub volume_percent: u8,
    /// Linear fade to silence ending on the last frame. Must be >= 0.
    /// Default [`DEFAULT_FADE_OUT`].
    pub fade_out: Micros,
    /// Linear fade from silence starting on the first frame. Must be >= 0.
    /// Default 0 (no fade-in).
    pub fade_in: Micros,
}

impl MusicBedOptions {
    /// Options for a bed of `length` at `sample_rate` Hz with `channels`
    /// channels; the other fields take their defaults (start 0, volume 100%,
    /// fade-out [`DEFAULT_FADE_OUT`], no fade-in).
    pub fn new(sample_rate: u32, channels: u16, length: Micros) -> Self {
        Self {
            sample_rate,
            channels,
            length,
            start: Micros(0),
            volume_percent: 100,
            fade_out: DEFAULT_FADE_OUT,
            fade_in: Micros(0),
        }
    }

    fn validate(&self) -> Result<(), Error> {
        if self.sample_rate == 0 {
            return Err(Error::InvalidOptions("sample_rate must be > 0"));
        }
        if !(1..=2).contains(&self.channels) {
            return Err(Error::InvalidOptions("channels must be 1 or 2"));
        }
        if self.length.0 < 0 || self.start.0 < 0 || self.fade_out.0 < 0 || self.fade_in.0 < 0 {
            return Err(Error::InvalidOptions(
                "length, start and fades must not be negative",
            ));
        }
        Ok(())
    }
}

/// Interleaved PCM audio ready to hand to an encoder.
#[derive(Debug, Clone, PartialEq)]
pub struct PcmAudio {
    /// Interleaved samples (`channels` per frame), in [-1, 1].
    pub samples: Vec<f32>,
    /// Sample rate in Hz.
    pub sample_rate: u32,
    /// Number of channels per frame.
    pub channels: u16,
}

impl PcmAudio {
    /// Number of frames (samples per channel).
    pub fn frames(&self) -> usize {
        if self.channels == 0 {
            0
        } else {
            self.samples.len() / usize::from(self.channels)
        }
    }
}

/// Decode the music file at `path` and render the music bed described by
/// `options`.
///
/// - Channels: a mono source is duplicated to both stereo channels; stereo is
///   kept as is; a source with more than 2 channels is downmixed to stereo
///   by a simple average: left is the mean of every left-side channel
///   (front, side, rear, ... left) and every centre channel, right likewise
///   with the right-side channels; LFE is dropped. For mono output all
///   source channels are averaged.
/// - Timing: the bed starts `options.start` into the song and has exactly
///   `round(length * sample_rate)` frames; past the end of the song it is
///   silence (no looping). Resampled with `rubato` when the song's rate
///   differs from `options.sample_rate`.
/// - Level: linear gain `volume_percent / 100`, then linear fade-in and
///   fade-out at the bed's edges, then clamped to [-1, 1].
pub fn render_music_bed(
    path: impl AsRef<Path>,
    options: &MusicBedOptions,
) -> Result<PcmAudio, Error> {
    options.validate()?;
    let decoded = decode::decode_interleaved(path.as_ref())?;
    render_decoded(decoded, options)
}

/// [`render_music_bed`] on already decoded audio. Options must be valid.
pub(crate) fn render_decoded(
    decoded: DecodedAudio,
    options: &MusicBedOptions,
) -> Result<PcmAudio, Error> {
    let channels = usize::from(options.channels);
    let src_rate = decoded.sample_rate;
    let source = map_channels(&decoded, channels);
    let src_frames = source.len() / channels;

    let out_frames = micros_to_frames(options.length, options.sample_rate);
    let mut samples = vec![0.0f32; out_frames * channels];

    if src_rate == options.sample_rate {
        let first = micros_to_frames(options.start, src_rate);
        let n = src_frames.saturating_sub(first).min(out_frames);
        if n > 0 {
            samples[..n * channels]
                .copy_from_slice(&source[first * channels..(first + n) * channels]);
        }
    } else {
        // Resample only the range the bed needs, plus a margin for filter
        // context, then cut the exact output frames out of it.
        let ratio = f64::from(options.sample_rate) / f64::from(src_rate);
        let start = options.start.as_secs_f64() * f64::from(src_rate);
        let first = (start.floor() as usize).saturating_sub(RESAMPLE_MARGIN);
        let last = ((start + out_frames as f64 / ratio).ceil() as usize)
            .saturating_add(RESAMPLE_MARGIN)
            .min(src_frames);
        if first < last {
            let resampled = decode::resample_interleaved(
                &source[first * channels..last * channels],
                channels,
                src_rate,
                options.sample_rate,
            )?;
            let offset = ((start - first as f64) * ratio).round() as usize;
            let available = resampled.len() / channels;
            let n = available.saturating_sub(offset).min(out_frames);
            if n > 0 {
                samples[..n * channels]
                    .copy_from_slice(&resampled[offset * channels..(offset + n) * channels]);
            }
        }
    }

    apply_level(&mut samples, channels, options);
    Ok(PcmAudio {
        samples,
        sample_rate: options.sample_rate,
        channels: options.channels,
    })
}

/// Interleaved samples of `decoded` with `channels` (1 or 2) channels; see
/// [`render_music_bed`] for the mapping.
fn map_channels(decoded: &DecodedAudio, channels: usize) -> Vec<f32> {
    let src = decoded.channels;
    if src == channels {
        return decoded.samples.clone();
    }
    if channels == 1 {
        return decoded.clone().into_mono().samples;
    }
    if src == 1 {
        return decoded.samples.iter().flat_map(|&s| [s, s]).collect();
    }
    let feeds = |want: Side| -> Vec<usize> {
        let idx: Vec<usize> = (0..src)
            .filter(|&i| decoded.sides[i] == want || decoded.sides[i] == Side::Both)
            .collect();
        if idx.is_empty() {
            // No channel on this side: fall back to every audible channel.
            (0..src)
                .filter(|&i| decoded.sides[i] != Side::Neither)
                .collect()
        } else {
            idx
        }
    };
    let (left, right) = (feeds(Side::Left), feeds(Side::Right));
    let mean = |frame: &[f32], idx: &[usize]| -> f32 {
        if idx.is_empty() {
            0.0
        } else {
            idx.iter().map(|&i| frame[i]).sum::<f32>() / idx.len() as f32
        }
    };
    decoded
        .samples
        .chunks_exact(src)
        .flat_map(|frame| [mean(frame, &left), mean(frame, &right)])
        .collect()
}

/// Apply gain, fades and clamping in place.
fn apply_level(samples: &mut [f32], channels: usize, options: &MusicBedOptions) {
    let gain = f32::from(options.volume_percent) / 100.0;
    let frames = samples.len() / channels;
    let fade_in = micros_to_frames(options.fade_in, options.sample_rate);
    let fade_out = micros_to_frames(options.fade_out, options.sample_rate);
    for (k, frame) in samples.chunks_exact_mut(channels).enumerate() {
        let mut g = gain;
        if fade_in > 0 && k < fade_in {
            g *= k as f32 / fade_in as f32;
        }
        let from_end = frames - 1 - k;
        if fade_out > 0 && from_end < fade_out {
            g *= from_end as f32 / fade_out as f32;
        }
        for s in frame {
            *s = (*s * g).clamp(-1.0, 1.0);
        }
    }
}

/// `round(t * rate)` frames for a non-negative `t`.
pub(crate) fn micros_to_frames(t: Micros, rate: u32) -> usize {
    let t = i128::from(t.0.max(0));
    ((t * i128::from(rate) + 500_000) / 1_000_000) as usize
}

#[cfg(test)]
mod tests {
    use super::*;
    use symphonia::core::audio::{Channels, Position};

    fn decoded(samples: Vec<f32>, channels: usize, sides: Vec<Side>) -> DecodedAudio {
        DecodedAudio {
            samples,
            sample_rate: 48_000,
            channels,
            sides,
        }
    }

    #[test]
    fn surround_5_1_downmixes_left_and_right_with_centre() {
        let layout = Channels::Positioned(
            Position::FRONT_LEFT
                | Position::FRONT_RIGHT
                | Position::FRONT_CENTER
                | Position::LFE1
                | Position::REAR_LEFT
                | Position::REAR_RIGHT,
        );
        let sides = decode::channel_sides(&layout, 6);
        assert_eq!(
            sides,
            [
                Side::Left,
                Side::Right,
                Side::Both,
                Side::Neither,
                Side::Left,
                Side::Right
            ]
        );
        // FL .3, FR .6, FC .9, LFE 1 (dropped), RL 0, RR .3
        let d = decoded(vec![0.3, 0.6, 0.9, 1.0, 0.0, 0.3], 6, sides);
        let out = map_channels(&d, 2);
        assert!((out[0] - 0.4).abs() < 1e-6, "left {}", out[0]);
        assert!((out[1] - 0.6).abs() < 1e-6, "right {}", out[1]);
    }

    #[test]
    fn discrete_channels_alternate_sides() {
        let sides = decode::channel_sides(&Channels::Discrete(4), 4);
        assert_eq!(sides, [Side::Left, Side::Right, Side::Left, Side::Right]);
        let d = decoded(vec![0.2, 0.4, 0.6, 0.8], 4, sides);
        assert_eq!(map_channels(&d, 2), vec![0.4, 0.6]);
    }

    #[test]
    fn stereo_to_mono_averages() {
        let d = decoded(vec![0.2, 0.4], 2, vec![Side::Left, Side::Right]);
        let out = map_channels(&d, 1);
        assert!((out[0] - 0.3).abs() < 1e-6);
    }

    #[test]
    fn fades_and_clamp() {
        let mut opts = MusicBedOptions::new(10, 1, Micros(1_000_000));
        opts.fade_in = Micros(500_000);
        opts.fade_out = Micros(500_000);
        opts.volume_percent = 200;
        let mut s = vec![0.75f32; 10];
        apply_level(&mut s, 1, &opts);
        // Fade-in 0, .2, .4, .6, .8 of gain 2 * .75 = 1.5, then clamped.
        let want = [0.0, 0.3, 0.6, 0.9, 1.0, 1.0, 0.9, 0.6, 0.3, 0.0];
        for (a, b) in s.iter().zip(want) {
            assert!((a - b).abs() < 1e-6, "{s:?}");
        }
    }

    #[test]
    fn rejects_invalid_options() {
        let d = || decoded(vec![0.0; 4], 2, vec![Side::Left, Side::Right]);
        for bad in [
            MusicBedOptions::new(0, 2, Micros(1)),
            MusicBedOptions::new(48_000, 3, Micros(1)),
            MusicBedOptions::new(48_000, 2, Micros(-1)),
        ] {
            assert!(matches!(bad.validate(), Err(Error::InvalidOptions(_))));
        }
        let ok = MusicBedOptions::new(48_000, 2, Micros(0));
        assert!(ok.validate().is_ok());
        assert_eq!(render_decoded(d(), &ok).unwrap().frames(), 0);
    }

    #[test]
    fn frame_rounding() {
        assert_eq!(micros_to_frames(Micros(1_000_000), 48_000), 48_000);
        assert_eq!(micros_to_frames(Micros(10), 48_000), 0); // 0.48 frames
        assert_eq!(micros_to_frames(Micros(11), 48_000), 1); // 0.528 frames
    }
}
