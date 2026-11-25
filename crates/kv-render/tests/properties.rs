//! Property tests: frame plans of random repacked EDLs are well formed.

use kv_core::{Edl, MediaId, Micros, Segment, Speed, Transition};
use kv_media::Rational;
use kv_render::plan;
use proptest::prelude::*;

const FPS: [Rational; 5] = [
    Rational::new(24, 1),
    Rational::new(25, 1),
    Rational::new(30000, 1001),
    Rational::new(60, 1),
    Rational::new(24000, 1001),
];

/// (media, src_in, source length, crossfade ms or 0, speed index)
type RawSeg = (u64, i64, i64, i64, usize);

fn raw_seg() -> impl Strategy<Value = RawSeg> {
    (
        1u64..5,
        0i64..10_000_000,
        1i64..3_000_000,
        prop_oneof![Just(0i64), 1i64..1_500],
        0usize..5,
    )
}

fn build(raw: &[RawSeg]) -> Edl {
    const SPEEDS: [u32; 5] = [100, 50, 200, 25, 150];
    let segments = raw
        .iter()
        .map(|&(m, src_in, len, xf_ms, sp)| Segment {
            media_id: MediaId(m),
            src_in: Micros(src_in),
            src_out: Micros(src_in + len),
            timeline_start: Micros(0),
            transition: if xf_ms == 0 {
                Transition::Cut
            } else {
                Transition::Crossfade {
                    duration: Micros(xf_ms * 1_000),
                }
            },
            speed: Speed(SPEEDS[sp]),
        })
        .collect();
    let mut edl = Edl { segments };
    edl.repack();
    edl
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn plans_are_well_formed(
        raw in prop::collection::vec(raw_seg(), 0..6),
        fps_i in 0usize..5,
        src_fps in prop::collection::vec(prop::option::of(0usize..5), 5),
    ) {
        let edl = build(&raw);
        let fps = FPS[fps_i];
        // Media 1..5: a source rate from the same list, or unknown.
        let source_fps = |m: MediaId| src_fps[m.0 as usize].map(|i| FPS[i]);
        let p = plan(&edl, fps, &source_fps).unwrap();

        // Frame count matches the EDL length within one frame.
        let end = edl
            .segments
            .iter()
            .map(|s| s.timeline_start.0 + s.timeline_len().0)
            .max()
            .unwrap_or(0);
        let expect = end as f64 * fps.as_f64() / 1e6;
        prop_assert!((p.len() as f64 - expect).abs() <= 1.0, "{} frames vs {expect}", p.len());

        let mut prev_pts = None;
        let mut count = 0u64;
        for f in p.iter() {
            prop_assert_eq!(f.index, count);
            count += 1;
            if let Some(prev) = prev_pts {
                prop_assert!(f.pts > prev);
            }
            prev_pts = Some(f.pts);
            prop_assert!(f.pts.0 < end);

            // Two segments in a crossfade, two source frames each in slow
            // motion.
            prop_assert!(f.layers.len() <= 4);
            let segs: std::collections::BTreeSet<usize> =
                f.layers.iter().map(|l| l.segment).collect();
            prop_assert!(segs.len() <= 2);
            if !edl.segments.is_empty() {
                // Repacked EDLs have no gaps.
                prop_assert!(!f.layers.is_empty(), "gap at {:?}", f.pts);
            }
            let mut sum = 0.0f32;
            for l in &f.layers {
                let s = &edl.segments[l.segment];
                prop_assert_eq!(l.media_id, s.media_id);
                prop_assert!(l.src_time >= s.src_in && l.src_time < s.src_out);
                prop_assert!(l.weight > 0.0 && l.weight <= 1.0);
                if s.speed.0 >= 100 {
                    prop_assert_eq!(f.layers.iter().filter(|m| m.segment == l.segment).count(), 1);
                }
                sum += l.weight;
            }
            if !f.layers.is_empty() {
                prop_assert!((sum - 1.0).abs() < 1e-6, "weights sum to {sum}");
            }
        }
        prop_assert_eq!(count, p.len());
    }
}
