//! kwikvid auto-edit: turn clips, a music analysis and user constraints into a
//! beat-synced edit decision list, like GoPro Quik's automatic edit.
//!
//! [`generate`] is a pure, deterministic function (no I/O, ADR 0001): the same
//! [`EditInput`] (including its seed) always gives the same [`Edl`].
//!
//! Algorithm v1:
//!
//! 1. **Beat grid.** Cuts may land only on the music's beats. The edit ends on
//!    the beat closest to the target length; music shorter than the target is
//!    continued at its median tempo.
//! 2. **Candidates.** Windows around HiLights rank first, then the
//!    highest-scoring windows (per-0.5 s scores), then evenly spread windows.
//!    A small seeded random bonus breaks ties, so different seeds give
//!    different edits. Excluded and pinned footage is never a candidate.
//! 3. **Pins** are hard constraints: each pin becomes one segment covering it,
//!    padded at its edges to the next beat (trimmed at its edges only when
//!    the surrounding footage leaves no room to pad).
//! 4. **Slots.** The remaining time is filled with the best candidates,
//!    preferring media used less so far. Slots are about 1 s where the music
//!    is louder than average, 2 s on average and 3 s where it is quieter,
//!    rounded to whole beats, ending on a downbeat when one is a beat away.
//!    Beat counts are assigned by dynamic programming so the edit ends exactly
//!    on the end beat, and the number of slots is nudged while that improves
//!    the fit.
//! 5. **Order.** Segments play in chronological order: media in input order,
//!    then by source time.
//!
//! Every segment is a plain cut at normal speed; transitions and speed ramps
//! are out of scope for v1.

mod candidates;
mod grid;
mod layout;
mod rng;

use kv_core::{Edl, MediaId, Micros, RangeRef};
use serde::{Deserialize, Serialize};

pub use kv_audio::MusicAnalysis;

use candidates::State;
use layout::{Item, Outcome};

/// Spacing of [`MediaSummary::scores`]: one score per 0.5 s of media.
pub const SCORE_INTERVAL: Micros = Micros(500_000);
/// Longest slot, in beats.
pub const MAX_SLOT_BEATS: usize = 16;
/// Longest target length accepted (1 hour).
pub const MAX_TARGET_LENGTH: Micros = Micros(3_600 * 1_000_000);
/// How many times the slot count is nudged by one to improve the fit.
const REFINE_STEPS: usize = 8;

/// What the auto-edit knows about one clip.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MediaSummary {
    pub id: MediaId,
    pub duration: Micros,
    /// HiLight tags, as offsets from the start of the media.
    pub hilights: Vec<Micros>,
    /// Optional highlight score per [`SCORE_INTERVAL`] from the start of the
    /// media; higher is better. Only relative values matter.
    pub scores: Option<Vec<f32>>,
}

/// Everything [`generate`] needs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EditInput {
    /// Clips, in the order the edit should follow.
    pub media: Vec<MediaSummary>,
    pub music: MusicAnalysis,
    pub target_length: Micros,
    /// Ranges that must appear in the edit.
    pub pins: Vec<RangeRef>,
    /// Ranges that must not appear in the edit.
    pub excludes: Vec<RangeRef>,
    pub seed: u64,
}

/// Why [`generate`] could not produce an edit.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum AutoEditError {
    #[error("target length must be positive and at most 1 hour, got {0:?}")]
    InvalidTargetLength(Micros),
    #[error("the music needs at least two beats")]
    NotEnoughBeats,
    #[error("media id {0:?} appears more than once")]
    DuplicateMedia(MediaId),
    #[error("media {media_id:?} has a non-positive duration {duration:?}")]
    InvalidMediaDuration { media_id: MediaId, duration: Micros },
    #[error("unknown media id {0:?}")]
    UnknownMedia(MediaId),
    #[error("range {0:?} is empty or outside its media")]
    InvalidRange(RangeRef),
    #[error("pin {pin:?} overlaps exclude {exclude:?}")]
    PinExcluded { pin: RangeRef, exclude: RangeRef },
    #[error("pin {pin:?} is longer than the target length {target:?}")]
    PinLongerThanTarget { pin: RangeRef, target: Micros },
    #[error("the pins together do not fit in the target length {target:?}")]
    PinsExceedTarget { target: Micros },
    #[error("pin {pin:?} has less footage around it than one beat")]
    PinRoomTooShort { pin: RangeRef },
    #[error("not enough usable footage to fill the target length {target:?}")]
    NotEnoughMedia { target: Micros },
}

/// Half-open span `[start, end)` in microseconds within one media item.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Span {
    pub start: i64,
    pub end: i64,
}

impl Span {
    pub(crate) fn len(self) -> i64 {
        self.end - self.start
    }
}

pub(crate) fn range_of(media_id: MediaId, s: Span) -> RangeRef {
    RangeRef {
        media_id,
        start: Micros(s.start),
        end: Micros(s.end),
    }
}

/// Sort and merge overlapping or touching spans.
fn merge(mut spans: Vec<Span>) -> Vec<Span> {
    spans.sort_by_key(|s| (s.start, s.end));
    let mut out: Vec<Span> = Vec::with_capacity(spans.len());
    for s in spans {
        match out.last_mut() {
            Some(l) if s.start <= l.end => l.end = l.end.max(s.end),
            _ => out.push(s),
        }
    }
    out
}

/// `spans` minus `holes`; both sorted and merged.
fn subtract(spans: &[Span], holes: &[Span]) -> Vec<Span> {
    let mut out = Vec::new();
    for &s in spans {
        let mut cur = s.start;
        for h in holes.iter().filter(|h| h.end > s.start && h.start < s.end) {
            if h.start > cur {
                out.push(Span {
                    start: cur,
                    end: h.start,
                });
            }
            cur = cur.max(h.end);
        }
        if cur < s.end {
            out.push(Span {
                start: cur,
                end: s.end,
            });
        }
    }
    out
}

/// Validate ranges and group them per media index (sorted, merged).
fn group_ranges(
    media: &[MediaSummary],
    ranges: &[RangeRef],
) -> Result<Vec<Vec<Span>>, AutoEditError> {
    let mut out = vec![Vec::new(); media.len()];
    for r in ranges {
        let i = media
            .iter()
            .position(|m| m.id == r.media_id)
            .ok_or(AutoEditError::UnknownMedia(r.media_id))?;
        if r.start.0 < 0 || r.start >= r.end || r.end > media[i].duration {
            return Err(AutoEditError::InvalidRange(*r));
        }
        out[i].push(Span {
            start: r.start.0,
            end: r.end.0,
        });
    }
    Ok(out.into_iter().map(merge).collect())
}

/// Produce the edit for `input`. Deterministic: the same input and seed give
/// an identical [`Edl`].
pub fn generate(input: &EditInput) -> Result<Edl, AutoEditError> {
    let target = input.target_length;
    if target.0 <= 0 || target > MAX_TARGET_LENGTH {
        return Err(AutoEditError::InvalidTargetLength(target));
    }
    let media = &input.media;
    for (i, m) in media.iter().enumerate() {
        if m.duration.0 <= 0 {
            return Err(AutoEditError::InvalidMediaDuration {
                media_id: m.id,
                duration: m.duration,
            });
        }
        if media[..i].iter().any(|o| o.id == m.id) {
            return Err(AutoEditError::DuplicateMedia(m.id));
        }
    }
    for p in &input.pins {
        if let Some(e) = input.excludes.iter().find(|e| e.overlaps(p)) {
            return Err(AutoEditError::PinExcluded {
                pin: *p,
                exclude: *e,
            });
        }
    }
    let excludes = group_ranges(media, &input.excludes)?;
    let pins = group_ranges(media, &input.pins)?;
    for p in &input.pins {
        if p.end.0 - p.start.0 > target.0 {
            return Err(AutoEditError::PinLongerThanTarget { pin: *p, target });
        }
    }

    let grid = grid::Grid::build(&input.music, target.0)?;
    let nominal = grid.nominal();
    let allowed: Vec<Vec<Span>> = media
        .iter()
        .zip(&excludes)
        .map(|(m, ex)| {
            subtract(
                &[Span {
                    start: 0,
                    end: m.duration.0,
                }],
                ex,
            )
        })
        .collect();
    let free: Vec<Vec<Span>> = allowed
        .iter()
        .zip(&pins)
        .map(|(a, p)| subtract(a, p))
        .collect();

    let mut rng = rng::Rng::new(input.seed);
    let cands = candidates::generate(media, &free, nominal, &mut rng);
    let mut state = vec![State::Unused; cands.len()];

    // Initial guess of the number of free slots: simulate slot lengths over
    // the beats the pins leave.
    let pin_beats: i64 = pins
        .iter()
        .flatten()
        .map(|p| (p.len() + grid.median_iv - 1) / grid.median_iv)
        .sum();
    let free_beats = (grid.n as i64 - pin_beats).max(0) as usize;
    let mut c = 0;
    let mut slots = 0;
    while c < free_beats {
        c += grid.snapped_slot_beats(c);
        slots += 1;
    }
    for _ in 0..slots {
        match candidates::pick_next(&cands, &state, nominal) {
            Some(i) => state[i] = State::Chosen,
            None => break,
        }
    }

    let room_of = |mi: usize, core: Span| -> Span {
        allowed[mi]
            .iter()
            .copied()
            .find(|a| a.start <= core.start && core.end <= a.end)
            .unwrap_or(core)
    };

    let build = |state: &[State]| -> Vec<Item> {
        let mut items: Vec<Item> = Vec::new();
        for (mi, ps) in pins.iter().enumerate() {
            for &p in ps {
                items.push(Item {
                    media: mi,
                    media_id: media[mi].id,
                    core: p,
                    is_pin: true,
                    room: room_of(mi, p),
                    cand: None,
                    phase: 0.5,
                });
            }
        }
        for (ci, cand) in cands.iter().enumerate() {
            if state[ci] == State::Chosen {
                let core = Span {
                    start: cand.center,
                    end: cand.center,
                };
                items.push(Item {
                    media: cand.media,
                    media_id: media[cand.media].id,
                    core,
                    is_pin: false,
                    room: room_of(cand.media, core),
                    cand: Some(ci),
                    phase: cand.phase,
                });
            }
        }
        layout::arrange(&mut items);
        items
    };
    let worst = |state: &[State]| -> Option<usize> {
        (0..cands.len())
            .filter(|&i| state[i] == State::Chosen)
            .min_by(|&a, &b| cands[a].score.total_cmp(&cands[b].score))
    };
    // Layout of `state` if it succeeds, for trying alternatives.
    let try_layout = |state: &[State]| match layout::layout(&grid, &build(state)) {
        Ok(Outcome::Done(segments, cost)) => Some((segments, cost)),
        _ => None,
    };

    let guard = 2 * cands.len() + 64;
    for _ in 0..guard {
        let items = build(&state);
        if items.is_empty() {
            return Err(AutoEditError::NotEnoughMedia { target });
        }
        match layout::layout(&grid, &items)? {
            Outcome::Done(segments, cost) => {
                // The slot count was a guess: keep adding or dropping one
                // candidate while that makes slot lengths fit the music better.
                let mut best = (segments, cost);
                for _ in 0..REFINE_STEPS {
                    let more = candidates::pick_next(&cands, &state, nominal).map(|i| {
                        let mut s = state.clone();
                        s[i] = State::Chosen;
                        s
                    });
                    let fewer = worst(&state).map(|i| {
                        let mut s = state.clone();
                        s[i] = State::Banned;
                        s
                    });
                    let better = [more, fewer]
                        .into_iter()
                        .flatten()
                        .find_map(|s| try_layout(&s).filter(|(_, c)| *c < best.1).map(|r| (s, r)));
                    match better {
                        Some((s, r)) => {
                            state = s;
                            best = r;
                        }
                        None => break,
                    }
                }
                let mut edl = Edl { segments: best.0 };
                edl.repack();
                return Ok(edl);
            }
            Outcome::NeedMore => match candidates::pick_next(&cands, &state, nominal) {
                Some(i) => state[i] = State::Chosen,
                None => return Err(AutoEditError::NotEnoughMedia { target }),
            },
            Outcome::TooMany => match worst(&state) {
                Some(i) => state[i] = State::Banned,
                None => return Err(AutoEditError::PinsExceedTarget { target }),
            },
            Outcome::Unusable(i) => {
                if let Some(ci) = items[i].cand {
                    state[ci] = State::Banned;
                }
            }
        }
    }
    Err(AutoEditError::NotEnoughMedia { target })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(start: i64, end: i64) -> Span {
        Span { start, end }
    }

    #[test]
    fn merge_and_subtract() {
        assert_eq!(
            merge(vec![s(5, 8), s(0, 2), s(2, 3), s(7, 9)]),
            vec![s(0, 3), s(5, 9)]
        );
        assert_eq!(
            subtract(&[s(0, 10)], &[s(0, 2), s(4, 5), s(9, 12)]),
            vec![s(2, 4), s(5, 9)]
        );
    }
}
