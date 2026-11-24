//! Property tests for `generate`: length, beat alignment, pins, excludes and
//! media bounds hold for random inputs.

mod common;

use common::*;
use kv_autoedit::{generate, AutoEditError, EditInput};
use kv_core::{Micros, RangeRef};
use proptest::prelude::*;

/// Clearance kept around pins (from clip edges and excludes), in seconds.
const CLEAR: f64 = 2.5;

prop_compose! {
    fn arb_input()(
        bpm in 75.0f64..200.0,
        first_frac in 0.0f64..1.0,
        jitter in proptest::collection::vec(-15_000i64..15_000, 200),
        target in 5.0f64..40.0,
        extra_clips in 0usize..4,
        clip_secs in proptest::collection::vec(10.0f64..30.0, 20),
        hilight_fracs in proptest::collection::vec(proptest::collection::vec(0.0f64..1.0, 0..3), 20),
        scored in proptest::collection::vec(any::<bool>(), 20),
        pin_specs in proptest::collection::vec((0.0f64..1.0, 0.0f64..1.0), 0..3),
        exclude_specs in proptest::collection::vec((0.0f64..1.0, 1.0f64..5.0), 0..3),
        seed in any::<u64>(),
    ) -> EditInput {
        let iv = 60.0 / bpm;
        let mut m = music(bpm, target + 5.0, (first_frac * iv * 1e6) as i64, Vec::new());
        for (b, j) in m.beats.iter_mut().zip(&jitter) {
            b.0 = (b.0 + j).max(1);
        }
        m.downbeats = m.beats.iter().step_by(4).copied().collect();

        let n = ((3.0 * target / 10.0).ceil() as usize + 1).max(6) + extra_clips;
        let media: Vec<_> = (0..n)
            .map(|i| {
                let secs = clip_secs[i];
                let hl: Vec<f64> = hilight_fracs[i].iter().map(|f| f * secs).collect();
                let scores = scored[i].then(|| {
                    (0..(secs * 2.0) as usize).map(|k| ((k * 7 + i) % 10) as f32 / 10.0).collect()
                });
                clip(i as u64 + 1, secs, &hl, scores)
            })
            .collect();

        let mut inp = input(media, m, target, seed);
        // Pins go in the first clips, one per clip, with clearance all round.
        let max_pin = (target / 4.0).min(3.0);
        for (i, (pos, len)) in pin_specs.iter().enumerate() {
            let secs = inp.media[i].duration.as_secs_f64();
            let len = 0.3 + len * (max_pin - 0.3);
            let start = CLEAR + pos * (secs - 2.0 * CLEAR - len);
            inp.pins.push(range(i as u64 + 1, start, start + len));
        }
        // Excludes go in clips without pins.
        for (j, (pos, len)) in exclude_specs.iter().enumerate() {
            let i = 3 + j;
            let secs = inp.media[i].duration.as_secs_f64();
            let start = pos * (secs - len);
            inp.excludes.push(range(i as u64 + 1, start, start + len));
        }
        inp
    }
}

fn end_of(edl: &kv_core::Edl) -> i64 {
    edl.segments
        .last()
        .map(|s| s.timeline_start.0 + s.timeline_len().0)
        .unwrap_or(0)
}

fn covers(edl: &kv_core::Edl, p: &RangeRef) -> bool {
    edl.segments
        .iter()
        .any(|s| s.media_id == p.media_id && s.src_in <= p.start && p.end <= s.src_out)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn edit_invariants(inp in arb_input()) {
        let edl = generate(&inp).expect("generate");
        prop_assert!(!edl.segments.is_empty());

        // Total length equals the target within one beat.
        let max_iv = inp.music.beats.windows(2).map(|w| w[1].0 - w[0].0).max().unwrap();
        let total = end_of(&edl);
        prop_assert!((total - inp.target_length.0).abs() <= max_iv,
            "total {total} target {:?} beat {max_iv}", inp.target_length);

        // Back to back, and every cut (and the end) on a beat within 1 ms.
        let on_beat = |t: i64| inp.music.beats.iter().any(|b| (b.0 - t).abs() <= 1_000);
        let mut cursor = 0;
        for (i, s) in edl.segments.iter().enumerate() {
            prop_assert_eq!(s.timeline_start.0, cursor);
            if i > 0 {
                prop_assert!(on_beat(s.timeline_start.0), "cut at {:?}", s.timeline_start);
            }
            prop_assert!(s.src_in < s.src_out);
            cursor += s.timeline_len().0;
        }
        prop_assert!(on_beat(total));

        // Within media bounds, clear of excludes, no footage used twice.
        for s in &edl.segments {
            let m = inp.media.iter().find(|m| m.id == s.media_id).unwrap();
            prop_assert!(s.src_in.0 >= 0 && s.src_out <= m.duration);
            let r = RangeRef { media_id: s.media_id, start: s.src_in, end: s.src_out };
            for e in &inp.excludes {
                prop_assert!(!r.overlaps(e), "segment {r:?} overlaps exclude {e:?}");
            }
            for o in &edl.segments {
                let ro = RangeRef { media_id: o.media_id, start: o.src_in, end: o.src_out };
                prop_assert!(std::ptr::eq(s, o) || !r.overlaps(&ro), "{r:?} overlaps {ro:?}");
            }
        }

        // Every pin fully covered by one segment.
        for p in &inp.pins {
            prop_assert!(covers(&edl, p), "pin {p:?} not covered");
        }
    }

    #[test]
    fn deterministic(inp in arb_input()) {
        prop_assert_eq!(generate(&inp), generate(&inp));
    }

    #[test]
    fn pin_longer_than_target_is_an_error(inp in arb_input(), extra in 0.1f64..5.0) {
        let mut inp = inp;
        let t = inp.target_length.as_secs_f64();
        inp.media[0].duration = Micros::from_secs_f64(t + extra + 1.0);
        inp.pins = vec![range(1, 0.5, 0.5 + t + extra)];
        let is_err = matches!(generate(&inp), Err(AutoEditError::PinLongerThanTarget { .. }));
        prop_assert!(is_err);
    }
}
