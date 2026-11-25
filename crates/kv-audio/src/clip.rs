//! Clip audio for export: the sound recorded with each clip, placed on the
//! edit's timeline under the music.
//!
//! - [`decode_range`] decodes a source range of a clip's audio track (AAC in
//!   MP4/MOV, or any format the music decoder reads) at the export's rate and
//!   channel count.
//! - [`render_clip_audio`] places each segment's source audio at its
//!   `timeline_start`, following cuts, crossfades and speed (see its docs).
//! - [`mix_under_music`] adds the clip audio to the music bed and limits the
//!   sum so it never clips above 0 dBFS.

use std::f32::consts::FRAC_PI_2;
use std::path::{Path, PathBuf};

use kv_core::{Edl, MediaId, Micros, Segment, Speed, Transition};

use crate::bed::{micros_to_frames, render_decoded, RESAMPLE_MARGIN};
use crate::decode::decode_window;
use crate::{Error, MusicBedOptions, PcmAudio};

/// Fade applied to clip audio on each side of a hard cut (and at the start
/// and end of the edit), so the cut does not click: 10 ms. Shortened to half
/// the segment for very short segments.
pub const CUT_FADE: Micros = Micros(10_000);

/// Release time constant of the limiter in [`mix_under_music`]: 50 ms
/// (guess). Attack is instant.
pub const LIMITER_RELEASE: Micros = Micros(50_000);

/// A media item whose clip audio is on, and its file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClipAudioSource {
    pub media_id: MediaId,
    pub path: PathBuf,
}

/// Output format of [`render_clip_audio`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClipAudioOptions {
    /// Output sample rate in Hz (the encoder's). Must be > 0.
    pub sample_rate: u32,
    /// Output channels: 1 (mono) or 2 (stereo).
    pub channels: u16,
    /// Output length; the output has exactly `round(length * sample_rate)`
    /// frames. Must be >= 0. Normally the edit's length.
    pub length: Micros,
}

impl ClipAudioOptions {
    fn validate(&self) -> Result<(), Error> {
        validate_format(self.sample_rate, self.channels)?;
        if self.length.0 < 0 {
            return Err(Error::InvalidOptions("length must not be negative"));
        }
        Ok(())
    }
}

fn validate_format(sample_rate: u32, channels: u16) -> Result<(), Error> {
    if sample_rate == 0 {
        return Err(Error::InvalidOptions("sample_rate must be > 0"));
    }
    if !(1..=2).contains(&channels) {
        return Err(Error::InvalidOptions("channels must be 1 or 2"));
    }
    Ok(())
}

/// Decode `[start, end)` of the audio track of `path` to interleaved PCM at
/// `sample_rate` Hz with `channels` (1 or 2) channels: exactly
/// `round((end - start) * sample_rate)` frames.
///
/// The track is seeked to, not decoded from the start. Channels are mapped
/// like [`render_music_bed`](crate::render_music_bed) does, and the audio is
/// resampled with `rubato` when its rate differs. A file without an audio
/// track, and any part of the range past the end of the track, is silence
/// (not an error). Samples are clamped to [-1, 1].
///
/// Times are track timestamps: an MP4 edit list (e.g. the AAC encoder's
/// priming delay of about 21 ms) is not applied, because symphonia does not
/// read edit lists yet (#107).
pub fn decode_range(
    path: impl AsRef<Path>,
    start: Micros,
    end: Micros,
    sample_rate: u32,
    channels: u16,
) -> Result<PcmAudio, Error> {
    validate_format(sample_rate, channels)?;
    if start.0 < 0 || end.0 < start.0 {
        return Err(Error::InvalidOptions("need 0 <= start <= end"));
    }
    let length = Micros(end.0 - start.0);
    let mut opts = MusicBedOptions::new(sample_rate, channels, length);
    opts.fade_out = Micros(0);
    match decode_window(path.as_ref(), start, end, RESAMPLE_MARGIN)? {
        None => Ok(silence(sample_rate, channels, length)),
        Some(w) => {
            // Offset of `start` into the window.
            let rate = i128::from(w.audio.sample_rate);
            let first_us = i128::from(w.first_frame) * 1_000_000 / rate;
            opts.start = Micros((i128::from(start.0) - first_us).max(0) as i64);
            render_decoded(w.audio, &opts)
        }
    }
}

fn silence(sample_rate: u32, channels: u16, length: Micros) -> PcmAudio {
    PcmAudio {
        samples: vec![0.0; micros_to_frames(length, sample_rate) * usize::from(channels)],
        sample_rate,
        channels,
    }
}

/// Render the clip audio of `edl`: for each segment whose media is in
/// `sources` (the media with clip audio on), its source audio
/// ([`decode_range`] of `[src_in, src_out)`) placed at `timeline_start`.
///
/// - Crossfades: the outgoing segment fades out and the incoming one fades
///   in with equal-power curves (`cos` / `sin` of `w * pi / 2`, `w` the
///   video blend weight of the incoming segment) over the same window as the
///   video crossfade, so the summed power stays constant.
/// - Hard cuts: [`CUT_FADE`] (10 ms) linear fades on both sides of the cut,
///   inside each segment, and at the start and end of every segment that has
///   no crossfade there.
/// - Speed: only segments at normal speed play. Slow motion (and any other
///   speed) is muted (decision, reversible: pitch-shifted or stretched audio
///   sounds wrong; Quik mutes it, guess).
///
/// Overlapping segments add up, so a crossfade may slightly exceed full
/// scale; [`mix_under_music`] limits the final mix. The output has exactly
/// `round(options.length * sample_rate)` frames; audio past it is dropped.
pub fn render_clip_audio(
    edl: &Edl,
    sources: &[ClipAudioSource],
    options: &ClipAudioOptions,
) -> Result<PcmAudio, Error> {
    options.validate()?;
    place_segments(edl, options, &mut |seg| {
        let Some(src) = sources.iter().find(|s| s.media_id == seg.media_id) else {
            return Ok(None);
        };
        decode_range(
            &src.path,
            seg.src_in,
            seg.src_out,
            options.sample_rate,
            options.channels,
        )
        .map(Some)
        .map_err(|e| Error::ClipSource {
            media_id: src.media_id,
            path: src.path.clone(),
            source: Box::new(e),
        })
    })
}

/// A gain ramp over `[start, start + len)` frames.
#[derive(Debug, Clone, Copy)]
struct Ramp {
    start: i64,
    len: i64,
    equal_power: bool,
}

impl Ramp {
    /// Fade-in gain at frame `k`: 0 before, 1 after.
    fn fade_in(&self, k: i64) -> f32 {
        if k < self.start {
            return 0.0;
        }
        if self.len <= 0 || k >= self.start + self.len {
            return 1.0;
        }
        self.curve((k - self.start) as f32 / self.len as f32)
    }

    /// Fade-out gain at frame `k`: 1 before, 0 after.
    fn fade_out(&self, k: i64) -> f32 {
        if k < self.start {
            return 1.0;
        }
        if self.len <= 0 || k >= self.start + self.len {
            return 0.0;
        }
        self.curve(1.0 - (k - self.start) as f32 / self.len as f32)
    }

    fn curve(&self, x: f32) -> f32 {
        if self.equal_power {
            (x * FRAC_PI_2).sin()
        } else {
            x
        }
    }
}

fn end_of(seg: &Segment) -> i64 {
    seg.timeline_start.0.saturating_add(seg.timeline_len().0)
}

/// Length of the crossfade into `inc` from `out`: the crossfade duration,
/// shortened to the actual overlap (as the frame plan does). 0 for a cut.
fn crossfade_len(out: &Segment, inc: &Segment) -> i64 {
    match inc.transition {
        Transition::Crossfade { duration } => {
            duration.0.min(end_of(out) - inc.timeline_start.0).max(0)
        }
        Transition::Cut => 0,
    }
}

/// [`render_clip_audio`] with the decoding injected: `decode(seg)` returns
/// the segment's source audio in the output format, or `None` when its
/// clip audio is off. Pure apart from `decode`.
fn place_segments(
    edl: &Edl,
    options: &ClipAudioOptions,
    decode: &mut dyn FnMut(&Segment) -> Result<Option<PcmAudio>, Error>,
) -> Result<PcmAudio, Error> {
    let rate = options.sample_rate;
    let ch = usize::from(options.channels);
    let mut out = silence(rate, options.channels, options.length);
    let total = (out.samples.len() / ch) as i64;
    let frame = |t: i64| -> i64 {
        let n = i128::from(t) * i128::from(rate);
        i64::try_from((n + 500_000).div_euclid(1_000_000)).unwrap_or(i64::MAX)
    };
    let cut_fade = frame(CUT_FADE.0);
    let segs = &edl.segments;
    for (i, seg) in segs.iter().enumerate() {
        if seg.speed != Speed::NORMAL || seg.src_out.0 <= seg.src_in.0 {
            continue;
        }
        let (a, mut b) = (frame(seg.timeline_start.0), frame(end_of(seg)));
        if a >= total || b <= a {
            continue;
        }
        let prev = i.checked_sub(1).map(|p| &segs[p]);
        let next = segs.get(i + 1);

        let short_fade = |a: i64, b: i64| cut_fade.min((b - a) / 2);
        let fade_in = match prev.map(|p| crossfade_len(p, seg)) {
            Some(x) if x > 0 => Ramp {
                start: a,
                len: frame(seg.timeline_start.0 + x) - a,
                equal_power: true,
            },
            _ => Ramp {
                start: a,
                len: short_fade(a, b),
                equal_power: false,
            },
        };
        let fade_out = match next {
            Some(n) if crossfade_len(seg, n) > 0 => {
                let s = frame(n.timeline_start.0);
                Ramp {
                    start: s,
                    len: frame(n.timeline_start.0 + crossfade_len(seg, n)) - s,
                    equal_power: true,
                }
            }
            other => {
                // A later segment starting inside this one without a
                // crossfade takes over at its start (as the picture does).
                if let Some(n) = other {
                    b = b.min(frame(n.timeline_start.0).max(a));
                }
                let len = short_fade(a, b);
                Ramp {
                    start: b - len,
                    len,
                    equal_power: false,
                }
            }
        };

        let Some(pcm) = decode(seg)? else {
            continue;
        };
        if pcm.channels != options.channels {
            return Err(Error::InvalidOptions(
                "decoded clip audio has the wrong channels",
            ));
        }
        let n = (pcm.frames() as i64).min(b - a).min(total - a);
        for j in 0..n.max(0) {
            let k = a + j;
            let g = fade_in.fade_in(k) * fade_out.fade_out(k);
            if g == 0.0 {
                continue;
            }
            let src = &pcm.samples[j as usize * ch..(j as usize + 1) * ch];
            let dst = &mut out.samples[k as usize * ch..(k as usize + 1) * ch];
            for (d, s) in dst.iter_mut().zip(src) {
                *d += s * g;
            }
        }
    }
    Ok(out)
}

/// The export's audio: `music + clip`, sample by sample, through a peak
/// limiter so no sample exceeds full scale (0 dBFS).
///
/// `music` is the rendered music bed (already at the project's music
/// volume), `clip` the output of [`render_clip_audio`] at gain 1. Both must
/// have the same rate and channels; the shorter one is padded with silence.
/// The limiter has an instant attack and a [`LIMITER_RELEASE`] release, so
/// audio that never exceeds full scale passes through unchanged.
pub fn mix_under_music(music: &PcmAudio, clip: &PcmAudio) -> Result<PcmAudio, Error> {
    if music.sample_rate != clip.sample_rate || music.channels != clip.channels {
        return Err(Error::InvalidOptions(
            "music and clip audio must have the same format",
        ));
    }
    validate_format(music.sample_rate, music.channels)?;
    let len = music.samples.len().max(clip.samples.len());
    let mut samples: Vec<f32> = (0..len)
        .map(|i| music.samples.get(i).unwrap_or(&0.0) + clip.samples.get(i).unwrap_or(&0.0))
        .collect();
    limit(&mut samples, usize::from(music.channels), music.sample_rate);
    Ok(PcmAudio {
        samples,
        sample_rate: music.sample_rate,
        channels: music.channels,
    })
}

/// Peak limiter to full scale, in place: per frame, the gain drops at once
/// to `1 / peak` when the frame's peak exceeds 1, and recovers towards the
/// largest safe gain with time constant [`LIMITER_RELEASE`].
fn limit(samples: &mut [f32], channels: usize, sample_rate: u32) {
    let release = LIMITER_RELEASE.as_secs_f64() * f64::from(sample_rate);
    let coeff = (-1.0 / release.max(1.0)).exp() as f32;
    let mut gain = 1.0f32;
    for frame in samples.chunks_exact_mut(channels) {
        let peak = frame.iter().fold(0.0f32, |m, s| m.max(s.abs()));
        let target = if peak > 1.0 { 1.0 / peak } else { 1.0 };
        gain = if target < gain {
            target
        } else {
            target + (gain - target) * coeff
        };
        for s in frame {
            *s = (*s * gain).clamp(-1.0, 1.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: u32 = 1_000;

    fn seg(media: u64, src_in: i64, src_out: i64, transition: Transition, speed: u32) -> Segment {
        Segment {
            media_id: MediaId(media),
            src_in: Micros(src_in),
            src_out: Micros(src_out),
            timeline_start: Micros(0),
            transition,
            speed: Speed(speed),
        }
    }

    fn edl(segments: Vec<Segment>) -> Edl {
        let mut e = Edl { segments };
        e.repack();
        e
    }

    fn opts(length: i64) -> ClipAudioOptions {
        ClipAudioOptions {
            sample_rate: RATE,
            channels: 1,
            length: Micros(length),
        }
    }

    /// Media `n` with clip audio on (`on` lists them) decodes to a constant
    /// `n / 10`, so the output is the gain envelope times that level.
    fn render(e: &Edl, on: &[u64], length: i64) -> Vec<f32> {
        let o = opts(length);
        place_segments(e, &o, &mut |s| {
            let frames = micros_to_frames(Micros(s.src_out.0 - s.src_in.0), RATE);
            Ok(on.contains(&s.media_id.0).then(|| PcmAudio {
                samples: vec![s.media_id.0 as f32 / 10.0; frames],
                sample_rate: RATE,
                channels: 1,
            }))
        })
        .unwrap()
        .samples
    }

    #[test]
    fn hard_cuts_fade_10ms_each_side() {
        // Two 1 s cuts at 1 kHz: 1000 frames each.
        let e = edl(vec![
            seg(1, 0, 1_000_000, Transition::Cut, 100),
            seg(2, 0, 1_000_000, Transition::Cut, 100),
        ]);
        let x = render(&e, &[1, 2], 2_000_000);
        assert_eq!(x.len(), 2_000);
        // 10 ms = 10 frames: linear fade-in from 0 at the very start.
        assert_eq!(x[0], 0.0);
        assert!((x[5] - 0.05).abs() < 1e-6, "{}", x[5]);
        assert!((x[10] - 0.1).abs() < 1e-6);
        assert!((x[500] - 0.1).abs() < 1e-6);
        // Fade-out ends just before the cut, fade-in starts on it.
        assert!(x[999] < 0.011 && x[1_000] == 0.0, "{} {}", x[999], x[1_000]);
        assert!((x[990] - 0.1).abs() < 1e-6 && x[989] > 0.099);
        assert!((x[1_010] - 0.2).abs() < 1e-6);
        assert!((x[1_995] - 0.2 * 5.0 / 10.0).abs() < 1e-6);
    }

    #[test]
    fn crossfade_is_equal_power_over_the_video_window() {
        // Media 1 (level 0.1) then media 2 (0.2) with a 400 ms crossfade:
        // segment 2 starts at 600 ms.
        let fade = Transition::Crossfade {
            duration: Micros(400_000),
        };
        let e = edl(vec![
            seg(1, 0, 1_000_000, Transition::Cut, 100),
            seg(2, 0, 1_000_000, fade, 100),
        ]);
        let only1 = render(&e, &[1], 1_600_000);
        let only2 = render(&e, &[2], 1_600_000);
        let both = render(&e, &[1, 2], 1_600_000);
        for k in 600..1_000 {
            let w = (k - 600) as f32 / 400.0;
            let g_out = only1[k] / 0.1;
            let g_in = only2[k] / 0.2;
            assert!((g_out - (w * FRAC_PI_2).cos()).abs() < 1e-5, "out at {k}");
            assert!((g_in - (w * FRAC_PI_2).sin()).abs() < 1e-5, "in at {k}");
            let power = g_out * g_out + g_in * g_in;
            assert!((power - 1.0).abs() < 1e-5, "power at {k}");
            assert!((both[k] - only1[k] - only2[k]).abs() < 1e-6, "sum at {k}");
        }
        // Midpoint: both at equal power.
        assert!((only1[800] / 0.1 - only2[800] / 0.2).abs() < 1e-6);
        // Before the fade: no 10 ms cut fade on the outgoing side.
        assert!((only1[599] - 0.1).abs() < 1e-6);
        // After the fade the incoming segment plays at full level.
        assert!((only2[1_000] - 0.2).abs() < 1e-6);
        assert_eq!(only1[1_000..], vec![0.0; 600][..]);
    }

    #[test]
    fn slow_motion_and_clip_audio_off_are_silent() {
        let e = edl(vec![
            seg(1, 0, 500_000, Transition::Cut, 50), // 1 s on the timeline
            seg(2, 0, 500_000, Transition::Cut, 100),
            seg(3, 0, 500_000, Transition::Cut, 100),
        ]);
        let mut asked = Vec::new();
        let x = place_segments(&e, &opts(2_000_000), &mut |s| {
            asked.push(s.media_id.0);
            Ok((s.media_id.0 != 3).then(|| PcmAudio {
                samples: vec![0.5; 500],
                sample_rate: RATE,
                channels: 1,
            }))
        })
        .unwrap()
        .samples;
        assert_eq!(asked, [2, 3], "slow motion is not even decoded");
        assert!(x[..1_000].iter().all(|&v| v == 0.0), "slow-mo muted");
        assert!((x[1_250] - 0.5).abs() < 1e-6);
        assert!(x[1_500..].iter().all(|&v| v == 0.0), "clip audio off");
    }

    #[test]
    fn output_length_is_exact_and_audio_past_it_is_dropped() {
        let e = edl(vec![seg(1, 0, 2_000_000, Transition::Cut, 100)]);
        let x = render(&e, &[1], 1_500_000);
        assert_eq!(x.len(), 1_500);
        assert!((x[1_499] - 0.1).abs() < 1e-6, "cut short, no fade-out");
        assert_eq!(render(&e, &[], 1_500_000), vec![0.0; 1_500]);
    }

    #[test]
    fn stereo_frames_stay_interleaved() {
        let e = edl(vec![seg(1, 0, 100_000, Transition::Cut, 100)]);
        let o = ClipAudioOptions {
            channels: 2,
            ..opts(100_000)
        };
        let x = place_segments(&e, &o, &mut |_| {
            Ok(Some(PcmAudio {
                samples: [0.25, -0.5].repeat(100),
                sample_rate: RATE,
                channels: 2,
            }))
        })
        .unwrap();
        assert_eq!(x.frames(), 100);
        assert_eq!(x.samples[100..102], [0.25, -0.5]);
    }

    #[test]
    fn mix_adds_and_limits_to_full_scale() {
        let pcm = |samples: Vec<f32>| PcmAudio {
            samples,
            sample_rate: 48_000,
            channels: 1,
        };
        // Below full scale: an exact sum, padded to the longer input.
        let m = mix_under_music(&pcm(vec![0.25, 0.5, -0.25]), &pcm(vec![0.25, -0.25])).unwrap();
        assert_eq!(m.samples, [0.5, 0.25, -0.25]);

        // 0.8 + 0.8 for 10 ms, then quiet: never above 1, recovers.
        let n = 48_000;
        let music = pcm(vec![0.8; n]);
        let mut c = vec![0.0; n];
        c[..480].fill(0.8);
        let m = mix_under_music(&music, &pcm(c)).unwrap();
        assert!(m.samples.iter().all(|s| s.abs() <= 1.0));
        assert!((m.samples[0] - 1.0).abs() < 1e-6, "limited to 0 dBFS");
        // The burst peaks at 1.6: gain 0.625. One release time constant
        // (50 ms) later the gain has recovered all but 1/e of the way, and
        // fully after 0.5 s.
        let g = m.samples[480 + 2_400] / 0.8;
        let want = 1.0 - 0.375 / std::f32::consts::E;
        assert!((g - want).abs() < 0.01, "gain {g}, want {want}");
        assert!((m.samples[n - 1] - 0.8).abs() < 1e-4);

        let stereo = PcmAudio {
            channels: 2,
            ..pcm(vec![0.0; 4])
        };
        assert!(matches!(
            mix_under_music(&pcm(vec![0.0; 4]), &stereo),
            Err(Error::InvalidOptions(_))
        ));
    }

    #[test]
    fn rejects_invalid_options() {
        let e = Edl::default();
        for bad in [
            ClipAudioOptions {
                sample_rate: 0,
                ..opts(0)
            },
            ClipAudioOptions {
                channels: 3,
                ..opts(0)
            },
            opts(-1),
        ] {
            assert!(matches!(
                render_clip_audio(&e, &[], &bad),
                Err(Error::InvalidOptions(_))
            ));
        }
        assert!(matches!(
            decode_range("x.wav", Micros(2), Micros(1), 48_000, 2),
            Err(Error::InvalidOptions(_))
        ));
        let empty = render_clip_audio(&e, &[], &opts(10_000)).unwrap();
        assert_eq!(empty.frames(), 10);
    }
}
