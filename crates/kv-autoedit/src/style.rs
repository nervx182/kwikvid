//! The style pass: turn the plain-cut edit from [`generate`](crate::generate)
//! into a styled one ([`StylePreset`]) without moving a cut or changing the
//! edit's length.
//!
//! [`apply_style`] is pure and deterministic, and needs no seed. It expects a
//! plain-cut edit at normal speed, laid out back to back from zero (what
//! `generate` returns). A *cut point* is the timeline time where one segment
//! hands over to the next: `timeline_start` of every segment after the first.
//!
//! - **Cut**: the edit is returned unchanged.
//! - **Smooth**: every segment after the first fades in with a
//!   [`Transition::Crossfade`] of half a beat at the local tempo (the mean
//!   of the beat intervals either side of the beat nearest the cut), clamped
//!   to [`MIN_CROSSFADE`]..=[`MAX_CROSSFADE`], then to half of either
//!   neighbour's timeline length, then to twice the spare footage each side
//!   has (see below), and rounded down to an even number of microseconds.
//!   The crossfade of duration `d` is centred on the cut point `c`: the
//!   outgoing segment's `src_out` moves `d / 2` later, the incoming
//!   segment's `src_in` moves `d / 2` earlier, and the incoming segment
//!   starts at `c - d / 2`, so the two overlap on `[c - d/2, c + d/2)`. The
//!   result is what [`Edl::repack`] gives, the first segment still starts at
//!   zero and the last still ends where it did. A crossfade that would come
//!   out shorter than [`MIN_CROSSFADE`] stays a cut.
//!   *Spare footage* before a segment reaches back to the media start, the
//!   end of an excluded range or halfway to the end of another segment of
//!   the same media (and after it, likewise forward), so a crossfade never
//!   shows excluded footage, footage outside the media, or footage another
//!   segment shows.
//! - **Action**: the segments that contain a HiLight of their media play at
//!   [`SLOW_MOTION`] (2x slow motion). If no segment contains a HiLight, the
//!   top-scoring third of the segments (mean [`MediaSummary::scores`] over
//!   the source range; unscored segments are never picked) are slowed
//!   instead. A slowed segment keeps its timeline place and length: its
//!   source range is halved, centred on its HiLights (the midpoint of the
//!   first and last one inside it) or on its best-scoring 0.5 s, and moved
//!   as little as needed to stay inside the original range and to keep
//!   covering every pinned footage the original covered. A segment whose
//!   pinned footage is longer than the halved range stays at normal speed.
//!   Slowed segments are picked in priority order (most HiLights, or highest
//!   score, then earliest) while the slow-motion total stays within
//!   [`MAX_SLOW_MOTION_PERCENT`] of the edit. Halving an odd source length
//!   rounds down, so a slowed segment can end up to 1 µs short of the next
//!   cut; cut points never move. Transitions stay cuts.
//!
//! Pins are hard constraints in every style: Smooth only extends source
//! ranges, and Action never drops footage of a pin.

use kv_core::{Edl, MediaId, Micros, RangeRef, Segment, Speed, StylePreset, Transition};

use crate::{MediaSummary, MusicAnalysis, SCORE_INTERVAL};

/// Shortest crossfade the Smooth style emits (guess).
pub const MIN_CROSSFADE: Micros = Micros(100_000);
/// Longest crossfade the Smooth style emits (guess).
pub const MAX_CROSSFADE: Micros = Micros(500_000);
/// Speed of the Action style's slow-motion segments.
pub const SLOW_MOTION: Speed = Speed(50);
/// Most of the edit's length that the Action style slows down, in percent
/// (guess).
pub const MAX_SLOW_MOTION_PERCENT: i64 = 40;
/// Beat interval assumed when the music has fewer than two beats (120 BPM).
const DEFAULT_BEAT: i64 = 500_000;

/// Apply `style` to `edl`, a plain-cut edit at normal speed from
/// [`generate`](crate::generate) for `media` and `music`. `pins` and
/// `excludes` are the section's constraints: pins stay covered, and no
/// excluded footage is added. See the [module docs](self) for the exact
/// rules.
pub fn apply_style(
    edl: &Edl,
    style: StylePreset,
    music: &MusicAnalysis,
    media: &[MediaSummary],
    pins: &[RangeRef],
    excludes: &[RangeRef],
) -> Edl {
    match style {
        StylePreset::Cut => edl.clone(),
        StylePreset::Smooth => smooth(edl, music, media, excludes),
        StylePreset::Action => action(edl, media, pins),
    }
}

fn to_src(timeline: i64, speed: Speed) -> i64 {
    let v = i128::from(timeline) * i128::from(speed.0.max(1)) / i128::from(Speed::NORMAL.0);
    i64::try_from(v).unwrap_or(i64::MAX)
}

fn to_timeline(src: i64, speed: Speed) -> i64 {
    let v = i128::from(src) * i128::from(Speed::NORMAL.0) / i128::from(speed.0.max(1));
    i64::try_from(v).unwrap_or(i64::MAX)
}

fn range_of(s: &Segment) -> RangeRef {
    RangeRef {
        media_id: s.media_id,
        start: s.src_in,
        end: s.src_out,
    }
}

/// Spare source footage before (`forward == false`) or after segment `i`.
fn spare(
    segs: &[Segment],
    i: usize,
    media: &[MediaSummary],
    excludes: &[RangeRef],
    forward: bool,
) -> i64 {
    let s = &segs[i];
    let id = s.media_id;
    let (a, b) = (s.src_in.0, s.src_out.0);
    let others = segs
        .iter()
        .enumerate()
        .filter(|&(j, o)| j != i && o.media_id == id)
        .map(|(_, o)| (o.src_in.0, o.src_out.0, true));
    let holes = excludes
        .iter()
        .filter(|e| e.media_id == id)
        .map(|e| (e.start.0, e.end.0, false));
    let blockers = others.chain(holes);
    if forward {
        let Some(duration) = media.iter().find(|m| m.id == id).map(|m| m.duration.0) else {
            return 0;
        };
        let mut hi = duration;
        for (start, end, is_seg) in blockers {
            if start >= b {
                hi = hi.min(if is_seg { b + (start - b) / 2 } else { start });
            } else if end > b {
                hi = b;
            }
        }
        (hi - b).max(0)
    } else {
        let mut lo = 0;
        for (start, end, is_seg) in blockers {
            if end <= a {
                lo = lo.max(if is_seg { end + (a - end + 1) / 2 } else { end });
            } else if start < a {
                lo = a;
            }
        }
        (a - lo).max(0)
    }
}

/// Beat interval at timeline time `t`: the mean of the intervals either side
/// of the nearest beat.
fn local_beat(beats: &[i64], t: i64) -> i64 {
    if beats.len() < 2 {
        return DEFAULT_BEAT;
    }
    let i = beats.partition_point(|&b| b < t);
    let k = if i == beats.len() || (i > 0 && t - beats[i - 1] <= beats[i] - t) {
        i - 1
    } else {
        i
    };
    let left = (k > 0).then(|| beats[k] - beats[k - 1]);
    let right = beats.get(k + 1).map(|&b| b - beats[k]);
    match (left, right) {
        (Some(l), Some(r)) => (l + r) / 2,
        (Some(x), None) | (None, Some(x)) => x,
        (None, None) => DEFAULT_BEAT,
    }
}

fn smooth(edl: &Edl, music: &MusicAnalysis, media: &[MediaSummary], excludes: &[RangeRef]) -> Edl {
    let segs = &edl.segments;
    let n = segs.len();
    let mut out = edl.clone();
    if n < 2 {
        return out;
    }
    let mut beats: Vec<i64> = music.beats.iter().map(|b| b.0).collect();
    beats.sort_unstable();
    beats.dedup();
    let len: Vec<i64> = segs.iter().map(|s| s.timeline_len().0).collect();
    let before: Vec<i64> = (0..n)
        .map(|i| to_timeline(spare(segs, i, media, excludes, false), segs[i].speed))
        .collect();
    let after: Vec<i64> = (0..n)
        .map(|i| to_timeline(spare(segs, i, media, excludes, true), segs[i].speed))
        .collect();

    // d[i]: crossfade into segment i (0 = cut).
    let mut d = vec![0i64; n + 1];
    for i in 1..n {
        let cut = segs[i].timeline_start.0;
        let mut x = (local_beat(&beats, cut) / 2)
            .clamp(MIN_CROSSFADE.0, MAX_CROSSFADE.0)
            .min(len[i - 1] / 2)
            .min(len[i] / 2)
            .min(after[i - 1].saturating_mul(2))
            .min(before[i].saturating_mul(2));
        x -= x % 2;
        if x >= MIN_CROSSFADE.0 {
            d[i] = x;
        }
    }

    for (i, seg) in out.segments.iter_mut().enumerate() {
        let (ext_in, ext_out) = (d[i] / 2, d[i + 1] / 2);
        seg.src_in = Micros(seg.src_in.0 - to_src(ext_in, seg.speed));
        seg.src_out = Micros(seg.src_out.0 + to_src(ext_out, seg.speed));
        seg.timeline_start = Micros(seg.timeline_start.0 - ext_in);
        seg.transition = if d[i] > 0 {
            Transition::Crossfade {
                duration: Micros(d[i]),
            }
        } else {
            Transition::Cut
        };
    }
    out
}

/// Mean score over `[a, b)` and the centre of the best-scoring interval.
fn score_of(m: &MediaSummary, a: i64, b: i64) -> Option<(f32, i64)> {
    let scores = m.scores.as_ref()?;
    let iv = SCORE_INTERVAL.0;
    let from = usize::try_from(a / iv).ok()?;
    let to = usize::try_from((b - 1) / iv + 1).ok()?.min(scores.len());
    let window = scores.get(from..to).filter(|w| !w.is_empty())?;
    let mean = window.iter().sum::<f32>() / window.len() as f32;
    let best = window
        .iter()
        .enumerate()
        .fold(0, |bi, (k, v)| if *v > window[bi] { k } else { bi });
    Some((mean, (from + best) as i64 * iv + iv / 2))
}

fn action(edl: &Edl, media: &[MediaSummary], pins: &[RangeRef]) -> Edl {
    let segs = &edl.segments;
    let mut out = edl.clone();
    let summary = |id: MediaId| media.iter().find(|m| m.id == id);
    let total = segs
        .iter()
        .map(|s| s.timeline_start.0 + s.timeline_len().0)
        .max()
        .unwrap_or(0);
    let budget = i128::from(total) * i128::from(MAX_SLOW_MOTION_PERCENT) / 100;

    // (segment index, focus in source time), in priority order.
    let hilit: Vec<(usize, usize, i64)> = segs
        .iter()
        .enumerate()
        .filter_map(|(i, s)| {
            let inside: Vec<i64> = summary(s.media_id)?
                .hilights
                .iter()
                .map(|h| h.0)
                .filter(|&h| s.src_in.0 <= h && h < s.src_out.0)
                .collect();
            let (lo, hi) = (*inside.iter().min()?, *inside.iter().max()?);
            Some((i, inside.len(), lo + (hi - lo) / 2))
        })
        .collect();
    let picks: Vec<(usize, i64)> = if hilit.is_empty() {
        let mut scored: Vec<(usize, f32, i64)> = segs
            .iter()
            .enumerate()
            .filter_map(|(i, s)| {
                let (mean, focus) = score_of(summary(s.media_id)?, s.src_in.0, s.src_out.0)?;
                Some((i, mean, focus))
            })
            .collect();
        scored.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
        scored.truncate(segs.len().div_ceil(3));
        scored.into_iter().map(|(i, _, f)| (i, f)).collect()
    } else {
        let mut hilit = hilit;
        hilit.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        hilit.into_iter().map(|(i, _, f)| (i, f)).collect()
    };

    let mut used: i128 = 0;
    for (i, focus) in picks {
        let s = &segs[i];
        if s.speed != Speed::NORMAL {
            continue;
        }
        let half = (s.src_out.0 - s.src_in.0) / 2;
        if half <= 0 {
            continue;
        }
        let slowed = to_timeline(half, SLOW_MOTION);
        if used + i128::from(slowed) > budget {
            continue;
        }
        // Pinned footage inside the segment must stay inside it.
        let r = range_of(s);
        let need = pins
            .iter()
            .filter(|p| p.overlaps(&r))
            .map(|p| (p.start.0.max(r.start.0), p.end.0.min(r.end.0)))
            .reduce(|a, b| (a.0.min(b.0), a.1.max(b.1)));
        let mut lo = s.src_in.0;
        let mut hi = s.src_out.0 - half;
        if let Some((ns, ne)) = need {
            lo = lo.max(ne - half);
            hi = hi.min(ns);
        }
        if lo > hi {
            continue;
        }
        let start = (focus - half / 2).clamp(lo, hi);
        let seg = &mut out.segments[i];
        seg.src_in = Micros(start);
        seg.src_out = Micros(start + half);
        seg.speed = SLOW_MOTION;
        used += i128::from(slowed);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const SEC: i64 = 1_000_000;

    fn seg(media: u64, src_in: i64, src_out: i64, start: i64) -> Segment {
        Segment {
            media_id: MediaId(media),
            src_in: Micros(src_in),
            src_out: Micros(src_out),
            timeline_start: Micros(start),
            transition: Transition::Cut,
            speed: Speed::NORMAL,
        }
    }

    fn clip(id: u64, secs: i64, hilights: &[i64], scores: Option<Vec<f32>>) -> MediaSummary {
        MediaSummary {
            id: MediaId(id),
            duration: Micros(secs * SEC),
            hilights: hilights.iter().map(|&h| Micros(h)).collect(),
            scores,
        }
    }

    fn music(iv: i64, count: i64) -> MusicAnalysis {
        let beats: Vec<Micros> = (1..=count).map(|k| Micros(k * iv)).collect();
        MusicAnalysis {
            duration: Micros(count * iv),
            tempo_bpm: 60e6 / iv as f64,
            downbeats: beats.iter().step_by(4).copied().collect(),
            beats,
            onsets: Vec::new(),
            energy: Vec::new(),
        }
    }

    /// Three 2 s segments from three 10 s clips, cuts at 2 s and 4 s.
    fn three() -> (Edl, Vec<MediaSummary>) {
        let edl = Edl {
            segments: vec![
                seg(1, 3 * SEC, 5 * SEC, 0),
                seg(2, 3 * SEC, 5 * SEC, 2 * SEC),
                seg(3, 3 * SEC, 5 * SEC, 4 * SEC),
            ],
        };
        let media = vec![
            clip(1, 10, &[], None),
            clip(2, 10, &[], None),
            clip(3, 10, &[], None),
        ];
        (edl, media)
    }

    #[test]
    fn cut_is_identity() {
        let (edl, media) = three();
        let m = music(500_000, 20);
        assert_eq!(
            apply_style(&edl, StylePreset::Cut, &m, &media, &[], &[]),
            edl
        );
    }

    #[test]
    fn smooth_centres_half_beat_crossfades_on_the_cuts() {
        let (edl, media) = three();
        // 120 BPM: half a beat is 250 ms.
        let out = apply_style(
            &edl,
            StylePreset::Smooth,
            &music(500_000, 20),
            &media,
            &[],
            &[],
        );
        let s = &out.segments;
        assert_eq!(s[0].transition, Transition::Cut);
        for (i, seg) in s.iter().enumerate().skip(1) {
            assert_eq!(
                seg.transition,
                Transition::Crossfade {
                    duration: Micros(250_000)
                }
            );
            assert_eq!(seg.timeline_start.0, i as i64 * 2 * SEC - 125_000);
        }
        assert_eq!(
            (s[0].src_in.0, s[0].src_out.0),
            (3 * SEC, 5 * SEC + 125_000)
        );
        assert_eq!(
            (s[1].src_in.0, s[1].src_out.0),
            (3 * SEC - 125_000, 5 * SEC + 125_000)
        );
        assert_eq!(
            (s[2].src_in.0, s[2].src_out.0),
            (3 * SEC - 125_000, 5 * SEC)
        );
        // Same layout as repack, same total length.
        let mut repacked = out.clone();
        repacked.repack();
        assert_eq!(repacked, out);
        assert_eq!(s[2].timeline_start.0 + s[2].timeline_len().0, 6 * SEC);
    }

    #[test]
    fn smooth_clamps_to_tempo_limits() {
        let (edl, media) = three();
        let fade = |iv| {
            apply_style(&edl, StylePreset::Smooth, &music(iv, 40), &media, &[], &[]).segments[1]
                .transition
        };
        // 300 BPM: 100 ms would be the floor anyway; 30 BPM: capped at 500 ms.
        assert_eq!(fade(150_000).overlap(), MIN_CROSSFADE);
        assert_eq!(fade(2 * SEC).overlap(), MAX_CROSSFADE);
    }

    #[test]
    fn smooth_respects_media_edges_excludes_and_shared_footage() {
        let (mut edl, mut media) = three();
        // Segment 1 starts at its media start: no room before it, so a cut.
        edl.segments[1].src_in = Micros(0);
        edl.segments[1].src_out = Micros(2 * SEC);
        // Segment 0 is followed by an exclude 60 ms after it: fade of 120 ms.
        let ex = RangeRef {
            media_id: MediaId(3),
            start: Micros(SEC),
            end: Micros(3 * SEC - 60_000),
        };
        media[2].duration = Micros(10 * SEC);
        let out = apply_style(
            &edl,
            StylePreset::Smooth,
            &music(500_000, 20),
            &media,
            &[],
            &[ex],
        );
        assert_eq!(out.segments[1].transition, Transition::Cut);
        assert_eq!(out.segments[2].transition.overlap(), Micros(120_000));
        assert_eq!(out.segments[2].src_in, ex.end);

        // Two segments of the same media 100 ms apart split the gap.
        let edl = Edl {
            segments: vec![seg(1, 0, 2 * SEC, 0), seg(1, 2_100_000, 4 * SEC, 2 * SEC)],
        };
        let out = apply_style(
            &edl,
            StylePreset::Smooth,
            &music(500_000, 20),
            &media,
            &[],
            &[],
        );
        assert_eq!(out.segments[1].transition.overlap(), Micros(100_000));
        assert_eq!(out.segments[0].src_out, out.segments[1].src_in);
    }

    #[test]
    fn smooth_too_short_neighbour_stays_a_cut() {
        let media = vec![clip(1, 10, &[], None), clip(2, 10, &[], None)];
        let edl = Edl {
            segments: vec![seg(1, SEC, SEC + 150_000, 0), seg(2, SEC, 3 * SEC, 150_000)],
        };
        let out = apply_style(
            &edl,
            StylePreset::Smooth,
            &music(500_000, 20),
            &media,
            &[],
            &[],
        );
        assert_eq!(out, edl);
    }

    #[test]
    fn action_slows_hilight_segments_around_the_hilight() {
        let (edl, mut media) = three();
        media[1].hilights = vec![Micros(4_800_000)];
        let out = apply_style(
            &edl,
            StylePreset::Action,
            &music(500_000, 20),
            &media,
            &[],
            &[],
        );
        let s = &out.segments[1];
        assert_eq!(s.speed, SLOW_MOTION);
        // Halved to 1 s, centred on 4.8 s but kept inside [3 s, 5 s).
        assert_eq!((s.src_in.0, s.src_out.0), (4 * SEC, 5 * SEC));
        assert_eq!(s.timeline_start, Micros(2 * SEC));
        assert_eq!(s.timeline_len(), Micros(2 * SEC));
        assert_eq!(out.segments[0], edl.segments[0]);
        assert_eq!(out.segments[2], edl.segments[2]);
    }

    #[test]
    fn action_caps_slow_motion_at_forty_percent() {
        let (edl, mut media) = three();
        for m in &mut media {
            m.hilights = vec![Micros(4 * SEC)];
        }
        // Each segment is a third of the edit: only one fits in 40%.
        let out = apply_style(
            &edl,
            StylePreset::Action,
            &music(500_000, 20),
            &media,
            &[],
            &[],
        );
        let slowed: Vec<usize> = (0..3)
            .filter(|&i| out.segments[i].speed == SLOW_MOTION)
            .collect();
        assert_eq!(slowed, vec![0]);
    }

    #[test]
    fn action_without_hilights_uses_top_scoring_third() {
        let (edl, mut media) = three();
        let flat = |v: f32| Some(vec![v; 20]);
        media[0].scores = flat(0.1);
        media[1].scores = flat(0.2);
        let mut peaky = vec![0.5; 20];
        peaky[9] = 0.9; // [4.5 s, 5 s)
        media[2].scores = Some(peaky);
        let out = apply_style(
            &edl,
            StylePreset::Action,
            &music(500_000, 20),
            &media,
            &[],
            &[],
        );
        let s: Vec<Speed> = out.segments.iter().map(|s| s.speed).collect();
        assert_eq!(s, vec![Speed::NORMAL, Speed::NORMAL, SLOW_MOTION]);
        assert_eq!(out.segments[2].src_out, Micros(5 * SEC));

        // No HiLights and no scores: nothing to slow down.
        let (edl, media) = three();
        let out = apply_style(
            &edl,
            StylePreset::Action,
            &music(500_000, 20),
            &media,
            &[],
            &[],
        );
        assert_eq!(out, edl);
    }

    #[test]
    fn action_keeps_pins_covered() {
        let (edl, mut media) = three();
        media[1].hilights = vec![Micros(4_800_000)];
        let pin = RangeRef {
            media_id: MediaId(2),
            start: Micros(3_200_000),
            end: Micros(3_900_000),
        };
        let out = apply_style(
            &edl,
            StylePreset::Action,
            &music(500_000, 20),
            &media,
            &[pin],
            &[],
        );
        let s = &out.segments[1];
        assert_eq!(s.speed, SLOW_MOTION);
        assert!(s.src_in <= pin.start && pin.end <= s.src_out);

        // A pin longer than the halved range: the segment keeps normal speed.
        let long = RangeRef {
            start: Micros(3_100_000),
            end: Micros(4_900_000),
            ..pin
        };
        let out = apply_style(
            &edl,
            StylePreset::Action,
            &music(500_000, 20),
            &media,
            &[long],
            &[],
        );
        assert_eq!(out, edl);
    }

    #[test]
    fn local_beat_uses_neighbouring_intervals() {
        let beats = [500_000, 1_000_000, 1_600_000, 2_400_000];
        assert_eq!(local_beat(&beats, 1_050_000), 550_000);
        assert_eq!(local_beat(&beats, 0), 500_000);
        assert_eq!(local_beat(&beats, 9_000_000), 800_000);
        assert_eq!(local_beat(&[1], 5), DEFAULT_BEAT);
    }
}
