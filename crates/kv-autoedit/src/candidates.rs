//! Candidate windows: places in the footage worth putting in the edit,
//! ranked HiLights first, then by highlight score, then spread evenly.

use crate::rng::Rng;
use crate::{MediaSummary, Span, SCORE_INTERVAL};

/// Score of a window around a HiLight, above any other kind of window.
const HILIGHT_SCORE: f32 = 2.0;
/// Score of a window in media without scores.
const NEUTRAL_SCORE: f32 = 0.5;
/// Largest seeded random bonus added to every candidate's score.
const NOISE: f32 = 0.3;
/// Score penalty per window already chosen from the same media.
const SPREAD_PENALTY: f32 = 0.4;

pub(crate) struct Candidate {
    /// Index into `EditInput::media`.
    pub media: usize,
    pub center: i64,
    pub score: f32,
    /// Fraction of the segment that plays before the centre (0.5 = centred);
    /// seeded so re-rolls vary the cut.
    pub phase: f32,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum State {
    Unused,
    Chosen,
    Banned,
}

/// Candidates from every span of `free` (per media: footage that is neither
/// excluded nor pinned). Centres are at least half a `nominal` window from
/// span edges and a whole `nominal` from each other's evenly spread
/// neighbours.
pub(crate) fn generate(
    media: &[MediaSummary],
    free: &[Vec<Span>],
    nominal: i64,
    rng: &mut Rng,
) -> Vec<Candidate> {
    let max_score = media
        .iter()
        .filter_map(|m| m.scores.as_ref())
        .flatten()
        .copied()
        .filter(|s| s.is_finite())
        .fold(0.0f32, f32::max);
    let max_score = if max_score > 0.0 { max_score } else { 1.0 };
    let half = nominal / 2;

    let mut out = Vec::new();
    for (mi, (m, spans)) in media.iter().zip(free).enumerate() {
        let window_score = |c: i64| -> Option<f32> {
            let scores = m.scores.as_ref()?;
            let from = ((c - half).max(0) / SCORE_INTERVAL.0) as usize;
            let to = ((c + half).max(0) as usize).div_ceil(SCORE_INTERVAL.0 as usize);
            let w = &scores[from.min(scores.len())..to.min(scores.len())];
            if w.is_empty() {
                return Some(0.0);
            }
            let sum: f32 = w
                .iter()
                .map(|s| if s.is_finite() { s.max(0.0) } else { 0.0 })
                .sum();
            Some(sum / w.len() as f32 / max_score)
        };
        let mut push = |center: i64, hilight: bool| {
            let ws = window_score(center);
            let base = if hilight {
                HILIGHT_SCORE + 0.25 * ws.unwrap_or(NEUTRAL_SCORE)
            } else {
                ws.unwrap_or(NEUTRAL_SCORE)
            };
            out.push(Candidate {
                media: mi,
                center,
                score: base + NOISE * rng.next_f32(),
                phase: 0.25 + 0.5 * rng.next_f32(),
            });
        };
        for s in spans {
            let len = s.len();
            let has_hilight = |a: i64, b: i64| m.hilights.iter().any(|h| (a..=b).contains(&h.0));
            if len < nominal {
                if len >= half {
                    push(s.start + len / 2, has_hilight(s.start, s.end));
                }
                continue;
            }
            let mut hs: Vec<i64> = m
                .hilights
                .iter()
                .map(|h| h.0)
                .filter(|&h| (s.start..=s.end).contains(&h))
                .map(|h| h.clamp(s.start + half, s.end - half))
                .collect();
            hs.sort_unstable();
            hs.dedup();
            for h in hs {
                push(h, true);
            }
            let count = len / nominal;
            let spacing = len / count;
            for j in 0..count {
                push(s.start + spacing * j + spacing / 2, false);
            }
        }
    }
    out
}

/// The best unused candidate that keeps a `nominal` distance from every
/// chosen one in the same media, preferring media used less so far.
pub(crate) fn pick_next(cands: &[Candidate], state: &[State], nominal: i64) -> Option<usize> {
    let chosen: Vec<&Candidate> = cands
        .iter()
        .zip(state)
        .filter(|(_, s)| **s == State::Chosen)
        .map(|(c, _)| c)
        .collect();
    let mut best: Option<(usize, f32)> = None;
    for (i, c) in cands.iter().enumerate() {
        if state[i] != State::Unused {
            continue;
        }
        let mut used = 0usize;
        let mut clash = false;
        for o in chosen.iter().filter(|o| o.media == c.media) {
            used += 1;
            clash |= (o.center - c.center).abs() < nominal;
        }
        if clash {
            continue;
        }
        let eff = c.score - SPREAD_PENALTY * used as f32;
        if best.is_none_or(|(_, b)| eff > b) {
            best = Some((i, eff));
        }
    }
    best.map(|(i, _)| i)
}
