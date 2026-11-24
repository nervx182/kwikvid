//! Lays chosen items (pins and candidate windows) out on the beat grid, in
//! order, so that every cut lands on a beat and the edit ends exactly at the
//! grid end.
//!
//! Dynamic programming over (item, start beat): each item takes a whole
//! number of beats that fits its room; free slots pay for straying from their
//! energy-based length and for not ending on a downbeat, pins pay for padding
//! beyond the first beat that covers them. The cheapest assignment that ends
//! exactly on the last grid point wins (ties go to the earliest choice, so
//! the result is deterministic).

use kv_core::{MediaId, Micros, Segment, Speed, Transition};

use crate::grid::Grid;
use crate::{AutoEditError, Span, MAX_SLOT_BEATS};

/// Cost of a free slot not ending on a downbeat, in beats of deviation: a
/// downbeat one beat away is preferred, two beats away is not.
const OFF_DOWNBEAT: f32 = 1.5;
/// Cost of trimming a pin that cannot be padded to a beat.
const TRIM_PIN: f32 = 4.0;

pub(crate) struct Item {
    pub media_id: MediaId,
    /// Index into `EditInput::media`.
    pub media: usize,
    /// The footage that must be shown: the pin, or the candidate centre as an
    /// empty span.
    pub core: Span,
    pub is_pin: bool,
    /// Where the segment may extend to: allowed footage, split halfway to the
    /// neighbouring items in the same media so segments never overlap.
    pub room: Span,
    /// Index of the candidate, for free items.
    pub cand: Option<usize>,
    /// Fraction of the segment before the core's middle (0.5 = centred).
    pub phase: f32,
}

pub(crate) enum Outcome {
    /// The segments and their total cost (lower fits the music better).
    Done(Vec<Segment>, f32),
    /// The items cannot fill the timeline; add another.
    NeedMore,
    /// The items do not fit on the timeline; drop a free one.
    TooMany,
    /// This free item's room is shorter than a beat wherever it could start.
    Unusable(usize),
}

/// Sort items chronologically (media order, then source time) and split the
/// room between neighbours in the same media.
pub(crate) fn arrange(items: &mut [Item]) {
    items.sort_by_key(|it| (it.media, it.core.start, it.core.end));
    for i in 1..items.len() {
        let (a, b) = items.split_at_mut(i);
        let (a, b) = (&mut a[i - 1], &mut b[0]);
        if a.media == b.media {
            let mid = a.core.end + (b.core.start - a.core.end) / 2;
            a.room.end = a.room.end.min(mid);
            b.room.start = b.room.start.max(mid);
        }
    }
}

/// The beat counts item `it` may take starting at point `c`, with the cost
/// of each. Returns false when its room is shorter than the beat at `c`.
fn options(g: &Grid, it: &Item, c: usize, out: &mut Vec<(usize, f32)>) -> bool {
    out.clear();
    let r = g.n - c;
    let room = it.room.len();
    let limit = if it.is_pin { r } else { r.min(MAX_SLOT_BEATS) };
    let kfit = (1..=limit)
        .take_while(|&k| g.dur(c, k) <= room)
        .last()
        .unwrap_or(0);
    if kfit == 0 {
        return false;
    }
    if it.is_pin {
        let need = it.core.len();
        match (1..=kfit).find(|&k| g.dur(c, k) >= need) {
            Some(kmin) => out.extend((kmin..=kfit).map(|k| (k, (k - kmin) as f32))),
            // Covering the pin needs more room than it has (not more timeline
            // than is left): trim its edges to the largest fit.
            None if g
                .points
                .get(c + kfit + 1)
                .is_some_and(|&p| p - g.points[c] > room) =>
            {
                out.push((kfit, TRIM_PIN))
            }
            None => {}
        }
    } else {
        let want = g.slot_beats(c) as f32;
        out.extend((1..=kfit).map(|k| {
            let off = if g.downbeat[c + k] { 0.0 } else { OFF_DOWNBEAT };
            (k, (k as f32 - want).abs() + off)
        }));
    }
    true
}

pub(crate) fn layout(g: &Grid, items: &[Item]) -> Result<Outcome, AutoEditError> {
    let n = g.n;
    const INF: f32 = f32::INFINITY;
    let mut cost = vec![INF; n + 1];
    cost[0] = 0.0;
    // back[i][c_end] = beats item i took to end at c_end.
    let mut back: Vec<Vec<u32>> = Vec::with_capacity(items.len());
    let mut opts = Vec::new();
    for (i, it) in items.iter().enumerate() {
        let mut next = vec![INF; n + 1];
        let mut from = vec![0u32; n + 1];
        // Whether some start had room for at least one beat.
        let mut any_room = false;
        let mut any_start = false;
        // Every later item needs at least one beat.
        let reserve = items.len() - i - 1;
        for (c, &here) in cost.iter().enumerate().take(n.saturating_sub(reserve)) {
            if here == INF {
                continue;
            }
            any_start = true;
            any_room |= options(g, it, c, &mut opts);
            for &(k, kc) in &opts {
                let e = c + k;
                if e + reserve > n {
                    break;
                }
                let total = here + kc;
                if total < next[e] {
                    next[e] = total;
                    from[e] = k as u32;
                }
            }
        }
        if next.iter().all(|&x| x == INF) {
            if any_room || !any_start {
                return Ok(Outcome::TooMany);
            }
            if it.is_pin {
                return Err(AutoEditError::PinRoomTooShort {
                    pin: crate::range_of(it.media_id, it.core),
                });
            }
            return Ok(Outcome::Unusable(i));
        }
        cost = next;
        back.push(from);
    }
    if cost[n] == INF {
        return Ok(Outcome::NeedMore);
    }

    // Walk back from the end to recover each item's beat count.
    let mut ks = vec![0usize; items.len()];
    let mut e = n;
    for i in (0..items.len()).rev() {
        ks[i] = back[i][e] as usize;
        e -= ks[i];
    }

    let mut segments = Vec::with_capacity(items.len());
    let mut c = 0;
    for (it, &k) in items.iter().zip(&ks) {
        let d = g.dur(c, k);
        let start = if d < it.core.len() {
            // A trimmed pin loses equal amounts at both edges.
            it.core.start + (it.core.len() - d) / 2
        } else {
            let lo = it.room.start.max(it.core.end - d);
            let hi = (it.room.end - d).min(it.core.start);
            let before = (d as f64 * f64::from(it.phase)) as i64;
            (it.core.start + it.core.len() / 2 - before).clamp(lo, hi)
        };
        segments.push(Segment {
            media_id: it.media_id,
            src_in: Micros(start),
            src_out: Micros(start + d),
            timeline_start: Micros(g.points[c]),
            transition: Transition::Cut,
            speed: Speed::NORMAL,
        });
        c += k;
    }
    Ok(Outcome::Done(segments, cost[n]))
}
