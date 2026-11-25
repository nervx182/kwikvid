//! The style pass on real `generate` output: golden snapshots per style,
//! property tests and `ReplaceEdl` validation.

mod common;

use common::*;
use kv_autoedit::style::{MAX_SLOW_MOTION_PERCENT, SLOW_MOTION};
use kv_autoedit::{apply_style, generate, EditInput};
use kv_core::{
    Command, Edl, MediaItem, MediaKind, Project, ProjectId, RangeRef, Speed, StylePreset,
    Transition,
};
use proptest::prelude::*;

const STYLES: [StylePreset; 3] = [StylePreset::Cut, StylePreset::Smooth, StylePreset::Action];

/// Clips with HiLights, a pin and an exclude, 120 BPM, 15 s edit.
fn scenario() -> EditInput {
    let mut inp = input(
        vec![
            clip(1, 20.0, &[4.0, 15.0], None),
            clip(2, 12.0, &[6.5], None),
            clip(3, 30.0, &[], None),
        ],
        music(120.0, 40.0, 250_000, Vec::new()),
        15.0,
        1,
    );
    inp.pins.push(range(3, 10.0, 11.2));
    inp.excludes.push(range(3, 20.0, 30.0));
    inp
}

fn style(inp: &EditInput, edl: &Edl, s: StylePreset) -> Edl {
    apply_style(edl, s, &inp.music, &inp.media, &inp.pins, &inp.excludes)
}

/// The EDL as a table with transitions and speeds.
fn styled_table(edl: &Edl) -> String {
    let f = |m: i64| format!("{:.3}", m as f64 / 1e6);
    let mut out = String::from(
        "  # | media | src_in  | src_out | start   | length | speed | transition\n\
         ----+-------+---------+---------+---------+--------+-------+-----------\n",
    );
    for (i, s) in edl.segments.iter().enumerate() {
        let tr = match s.transition {
            Transition::Cut => "cut".to_string(),
            Transition::Crossfade { duration } => format!("crossfade {}", f(duration.0)),
        };
        out.push_str(&format!(
            "{:>3} | {:>5} | {:>7} | {:>7} | {:>7} | {:>6} | {:>5} | {}\n",
            i,
            s.media_id.0,
            f(s.src_in.0),
            f(s.src_out.0),
            f(s.timeline_start.0),
            f(s.timeline_len().0),
            s.speed.0,
            tr
        ));
    }
    out.push_str(&format!("total {}\n", f(end_of(edl))));
    out
}

fn end_of(edl: &Edl) -> i64 {
    edl.segments
        .iter()
        .map(|s| s.timeline_start.0 + s.timeline_len().0)
        .max()
        .unwrap_or(0)
}

/// Cut points: the centre of each segment's transition into it.
fn cut_points(edl: &Edl) -> Vec<i64> {
    edl.segments
        .iter()
        .skip(1)
        .map(|s| s.timeline_start.0 + s.transition.overlap().0 / 2)
        .collect()
}

fn covers(edl: &Edl, p: &RangeRef) -> bool {
    edl.segments
        .iter()
        .any(|s| s.media_id == p.media_id && s.src_in <= p.start && p.end <= s.src_out)
}

/// `ReplaceEdl` on a project holding the scenario's media.
fn replace_edl(inp: &EditInput, edl: Edl) -> Result<(), kv_core::CoreError> {
    let mut project = Project::new(ProjectId(1));
    for m in &inp.media {
        project.apply(Command::AddMedia {
            item: MediaItem {
                id: m.id,
                path: format!("clip{}.mp4", m.id.0),
                duration: m.duration,
                kind: MediaKind::Video,
                hilights: m.hilights.clone(),
                clip_audio: false,
                chapters: Vec::new(),
            },
        })?;
    }
    project
        .apply(Command::ReplaceEdl {
            section: Project::DEFAULT_SECTION,
            edl,
        })
        .map(|_| ())
}

#[test]
fn golden_cut() {
    let inp = scenario();
    let edl = generate(&inp).unwrap();
    insta::assert_snapshot!(styled_table(&style(&inp, &edl, StylePreset::Cut)));
}

#[test]
fn golden_smooth() {
    let inp = scenario();
    let edl = generate(&inp).unwrap();
    insta::assert_snapshot!(styled_table(&style(&inp, &edl, StylePreset::Smooth)));
}

#[test]
fn golden_action() {
    let inp = scenario();
    let edl = generate(&inp).unwrap();
    insta::assert_snapshot!(styled_table(&style(&inp, &edl, StylePreset::Action)));
}

#[test]
fn styled_edits_pass_core_validation() {
    let inp = scenario();
    let edl = generate(&inp).unwrap();
    for s in STYLES {
        replace_edl(&inp, style(&inp, &edl, s)).unwrap_or_else(|e| panic!("{s:?}: {e}"));
    }
}

#[test]
fn smooth_matches_repack_and_action_slows_hilights() {
    let inp = scenario();
    let edl = generate(&inp).unwrap();
    let smooth = style(&inp, &edl, StylePreset::Smooth);
    assert!(smooth.segments[1..]
        .iter()
        .all(|s| matches!(s.transition, Transition::Crossfade { .. })));
    let mut repacked = smooth.clone();
    repacked.repack();
    assert_eq!(repacked, smooth);

    let action = style(&inp, &edl, StylePreset::Action);
    assert!(action.segments.iter().any(|s| s.speed == SLOW_MOTION));
    for s in action.segments.iter().filter(|s| s.speed == SLOW_MOTION) {
        let m = inp.media.iter().find(|m| m.id == s.media_id).unwrap();
        let orig = edl
            .segments
            .iter()
            .find(|o| o.timeline_start == s.timeline_start)
            .unwrap();
        assert!(m
            .hilights
            .iter()
            .any(|h| orig.src_in <= *h && *h < orig.src_out));
    }
}

prop_compose! {
    fn arb_input()(
        bpm in 75.0f64..200.0,
        first_frac in 0.0f64..1.0,
        jitter in proptest::collection::vec(-15_000i64..15_000, 200),
        target in 5.0f64..30.0,
        clip_secs in proptest::collection::vec(4.0f64..25.0, 12),
        hilight_fracs in proptest::collection::vec(proptest::collection::vec(0.0f64..1.0, 0..3), 12),
        scored in proptest::collection::vec(any::<bool>(), 12),
        pin in proptest::option::of((0.0f64..1.0, 0.0f64..1.0)),
        exclude in proptest::option::of((0.0f64..1.0, 0.5f64..3.0)),
        seed in any::<u64>(),
    ) -> EditInput {
        let iv = 60.0 / bpm;
        let mut m = music(bpm, target + 5.0, (first_frac * iv * 1e6) as i64, Vec::new());
        for (b, j) in m.beats.iter_mut().zip(&jitter) {
            b.0 = (b.0 + j).max(1);
        }
        m.downbeats = m.beats.iter().step_by(4).copied().collect();
        let n = ((3.0 * target / 10.0).ceil() as usize + 2).max(6);
        let media: Vec<_> = (0..n)
            .map(|i| {
                // The first clip is long enough to hold the pin with room.
                let secs = if i == 0 { clip_secs[i].max(12.0) } else { clip_secs[i] };
                let hl: Vec<f64> = hilight_fracs[i].iter().map(|f| f * secs).collect();
                let scores = scored[i].then(|| {
                    (0..(secs * 2.0) as usize).map(|k| ((k * 7 + i) % 10) as f32 / 10.0).collect()
                });
                clip(i as u64 + 1, secs, &hl, scores)
            })
            .collect();
        let mut inp = input(media, m, target, seed);
        if let Some((pos, len)) = pin {
            let secs = inp.media[0].duration.as_secs_f64();
            let len = 0.3 + len * 2.0;
            let start = 2.5 + pos * (secs - 5.0 - len);
            inp.pins.push(range(1, start, start + len));
        }
        if let Some((pos, len)) = exclude {
            let secs = inp.media[1].duration.as_secs_f64();
            let len = len.min(secs / 2.0);
            let start = pos * (secs - len);
            inp.excludes.push(range(2, start, start + len));
        }
        inp
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(48))]

    #[test]
    fn style_invariants(inp in arb_input()) {
        let edl = generate(&inp).expect("generate");
        let total = end_of(&edl);
        let cuts = cut_points(&edl);
        for s in STYLES {
            let out = style(&inp, &edl, s);
            prop_assert_eq!(&out, &style(&inp, &edl, s), "{:?} not deterministic", s);
            prop_assert_eq!(out.segments.len(), edl.segments.len());
            prop_assert!((end_of(&out) - total).abs() <= 1, "{:?} total {} vs {}", s, end_of(&out), total);
            prop_assert_eq!(out.segments[0].timeline_start.0, 0);
            let moved = cut_points(&out).iter().zip(&cuts).all(|(a, b)| a == b);
            prop_assert!(moved, "{:?} moved a cut", s);
            for (o, seg) in out.segments.iter().zip(&edl.segments) {
                let m = inp.media.iter().find(|m| m.id == seg.media_id).unwrap();
                prop_assert!(o.src_in.0 >= 0 && o.src_in < o.src_out && o.src_out <= m.duration);
                let r = RangeRef { media_id: o.media_id, start: o.src_in, end: o.src_out };
                for e in &inp.excludes {
                    prop_assert!(!r.overlaps(e), "{:?}: {:?} overlaps exclude {:?}", s, r, e);
                }
            }
            for p in inp.pins.iter().filter(|p| covers(&edl, p)) {
                prop_assert!(covers(&out, p), "{:?}: pin {:?} lost", s, p);
            }
            if s == StylePreset::Action {
                let slow: i64 = out.segments.iter().filter(|x| x.speed == SLOW_MOTION)
                    .map(|x| x.timeline_len().0).sum();
                prop_assert!(slow * 100 <= total * MAX_SLOW_MOTION_PERCENT);
                prop_assert!(out.segments.iter().all(|x| x.transition == Transition::Cut
                    && (x.speed == SLOW_MOTION || x.speed == Speed::NORMAL)));
            }
            if s == StylePreset::Cut {
                prop_assert_eq!(&out, &edl);
            }
            if s == StylePreset::Smooth {
                let mut repacked = out.clone();
                repacked.repack();
                prop_assert_eq!(&repacked, &out);
                for w in out.segments.windows(2) {
                    let a = RangeRef { media_id: w[0].media_id, start: w[0].src_in, end: w[0].src_out };
                    let b = RangeRef { media_id: w[1].media_id, start: w[1].src_in, end: w[1].src_out };
                    prop_assert!(!a.overlaps(&b), "{:?} overlaps {:?}", a, b);
                }
            }
            prop_assert!(replace_edl(&inp, out.clone()).is_ok(), "{:?} rejected", s);
        }
    }
}
