//! Golden snapshots for hand-built scenarios, behaviour checks, typed errors
//! and the speed budget.

mod common;

use std::time::Instant;

use common::*;
use kv_autoedit::{generate, AutoEditError, EditInput};
use kv_core::{MediaId, Micros};

/// Three clips with HiLights, steady 120 BPM, 15 s edit.
fn hilights_scenario() -> EditInput {
    input(
        vec![
            clip(1, 20.0, &[4.0, 15.0], None),
            clip(2, 12.0, &[6.5], None),
            clip(3, 30.0, &[], None),
        ],
        music(120.0, 40.0, 250_000, Vec::new()),
        15.0,
        1,
    )
}

/// Scored clips, no HiLights, one excluded range, music quiet then loud.
fn scores_scenario() -> EditInput {
    let ramp = |n: usize, peak: usize| -> Vec<f32> {
        (0..n)
            .map(|k| 1.0 - (k.abs_diff(peak) as f32 / n as f32))
            .collect()
    };
    let mut energy = vec![0.1f32; 100];
    energy.extend(vec![0.6f32; 200]);
    let mut inp = input(
        vec![
            clip(10, 18.0, &[], Some(ramp(36, 30))),
            clip(11, 25.0, &[], Some(ramp(50, 5))),
            clip(12, 15.0, &[], Some(ramp(30, 15))),
        ],
        music(96.0, 30.0, 400_000, energy),
        20.0,
        42,
    );
    inp.excludes.push(range(11, 0.0, 6.0));
    inp
}

/// Pins and an exclude over mixed clips, 100 BPM, 30 s edit.
fn pins_scenario() -> EditInput {
    let mut inp = input(
        vec![
            clip(1, 40.0, &[12.0], None),
            clip(
                2,
                25.0,
                &[],
                Some((0..50).map(|k| (k % 7) as f32).collect()),
            ),
            clip(3, 15.0, &[], None),
            clip(4, 8.0, &[2.0], None),
        ],
        music(100.0, 45.0, 0, Vec::new()),
        30.0,
        7,
    );
    inp.pins.push(range(1, 30.0, 33.3));
    inp.pins.push(range(3, 5.0, 6.0));
    inp.excludes.push(range(4, 0.0, 8.0));
    inp.excludes.push(range(1, 10.0, 14.0));
    inp
}

#[test]
fn golden_hilights() {
    let inp = hilights_scenario();
    insta::assert_snapshot!(table(&generate(&inp).unwrap(), &inp.music));
}

#[test]
fn golden_scores_and_exclude() {
    let inp = scores_scenario();
    insta::assert_snapshot!(table(&generate(&inp).unwrap(), &inp.music));
}

#[test]
fn golden_pins() {
    let inp = pins_scenario();
    insta::assert_snapshot!(table(&generate(&inp).unwrap(), &inp.music));
}

#[test]
fn every_hilight_is_used_when_there_is_time() {
    let inp = hilights_scenario();
    let edl = generate(&inp).unwrap();
    for m in &inp.media {
        for h in &m.hilights {
            assert!(
                edl.segments
                    .iter()
                    .any(|s| s.media_id == m.id && s.src_in <= *h && *h < s.src_out),
                "hilight {h:?} of {:?} unused",
                m.id
            );
        }
    }
}

#[test]
fn chronological_and_spread_across_clips() {
    let inp = scores_scenario();
    let edl = generate(&inp).unwrap();
    let key = |id: MediaId| inp.media.iter().position(|m| m.id == id).unwrap();
    assert!(edl
        .segments
        .windows(2)
        .all(|w| (key(w[0].media_id), w[0].src_in) < (key(w[1].media_id), w[1].src_in)));
    for m in &inp.media {
        assert!(edl.segments.iter().any(|s| s.media_id == m.id));
    }
}

#[test]
fn louder_music_gets_shorter_slots() {
    let inp = scores_scenario();
    let edl = generate(&inp).unwrap();
    let mean = |quiet: bool| {
        let lens: Vec<i64> = edl
            .segments
            .iter()
            .filter(|s| (s.timeline_start.0 < 10 * SEC) == quiet)
            .map(|s| s.timeline_len().0)
            .collect();
        lens.iter().sum::<i64>() as f64 / lens.len() as f64
    };
    assert!(
        mean(false) < mean(true),
        "loud {} quiet {}",
        mean(false),
        mean(true)
    );
}

#[test]
fn seeds_change_the_edit() {
    let mut inp = scores_scenario();
    let mut edls = Vec::new();
    for seed in 0..8 {
        inp.seed = seed;
        let edl = generate(&inp).unwrap();
        assert_eq!(edl, generate(&inp).unwrap());
        if !edls.contains(&edl) {
            edls.push(edl);
        }
    }
    assert!(
        edls.len() >= 5,
        "only {} distinct edits from 8 seeds",
        edls.len()
    );
}

#[test]
fn music_shorter_than_target_is_extended() {
    let mut inp = hilights_scenario();
    inp.music = music(120.0, 8.0, 500_000, Vec::new());
    let edl = generate(&inp).unwrap();
    let s = edl.segments.last().unwrap();
    assert_eq!(s.timeline_start.0 + s.timeline_len().0, 15 * SEC);
}

#[test]
fn pin_without_room_is_trimmed_at_its_edges() {
    // The whole 1.2 s clip is pinned; beats are 0.5 s, so the pin cannot be
    // padded and is trimmed to 1 s, centred.
    let mut inp = hilights_scenario();
    inp.media.push(clip(9, 1.2, &[], None));
    inp.pins.push(range(9, 0.0, 1.2));
    let edl = generate(&inp).unwrap();
    let s = edl
        .segments
        .iter()
        .find(|s| s.media_id == MediaId(9))
        .unwrap();
    assert_eq!((s.src_in, s.src_out), (Micros(100_000), Micros(1_100_000)));
}

#[test]
fn typed_errors() {
    let base = hilights_scenario();
    let err = |f: &dyn Fn(&mut EditInput)| {
        let mut inp = base.clone();
        f(&mut inp);
        generate(&inp).unwrap_err()
    };
    assert!(matches!(
        err(&|i| i.pins.push(range(1, 0.0, 16.0))),
        AutoEditError::PinLongerThanTarget { .. }
    ));
    assert!(matches!(
        err(&|i| {
            i.pins.push(range(1, 1.0, 3.0));
            i.excludes.push(range(1, 2.0, 5.0));
        }),
        AutoEditError::PinExcluded { .. }
    ));
    assert!(matches!(
        err(&|i| i.pins.extend([range(1, 0.0, 9.0), range(2, 0.0, 9.0)])),
        AutoEditError::PinsExceedTarget { .. }
    ));
    assert_eq!(
        err(&|i| i.excludes.push(range(99, 0.0, 1.0))),
        AutoEditError::UnknownMedia(MediaId(99))
    );
    assert!(matches!(
        err(&|i| i.excludes.push(range(1, 5.0, 25.0))),
        AutoEditError::InvalidRange(_)
    ));
    assert_eq!(
        err(&|i| i.music.beats.truncate(1)),
        AutoEditError::NotEnoughBeats
    );
    assert!(matches!(
        err(&|i| i.target_length = Micros(0)),
        AutoEditError::InvalidTargetLength(_)
    ));
    assert!(matches!(
        err(&|i| {
            i.media.truncate(1);
            i.media[0].duration = Micros(3 * SEC);
        }),
        AutoEditError::NotEnoughMedia { .. }
    ));
}

/// 50 clips and a 60 s target: under 50 ms in release builds.
#[test]
fn fifty_clips_sixty_seconds_is_fast() {
    let media = (0..50u64)
        .map(|i| {
            let secs = 20.0 + (i % 7) as f64 * 10.0;
            let hl: Vec<f64> = (0..i % 3).map(|k| secs * (k as f64 + 1.0) / 4.0).collect();
            let scores = (i % 2 == 0).then(|| {
                (0..(secs * 2.0) as usize)
                    .map(|k| ((k as u64 * 31 + i) % 17) as f32)
                    .collect()
            });
            clip(i + 1, secs, &hl, scores)
        })
        .collect();
    let mut inp = input(media, music(128.0, 180.0, 200_000, Vec::new()), 60.0, 3);
    inp.pins.push(range(5, 3.0, 5.0));
    inp.excludes.push(range(8, 0.0, 10.0));

    generate(&inp).unwrap();
    let runs = 5;
    let start = Instant::now();
    for _ in 0..runs {
        generate(&inp).unwrap();
    }
    let per_run = start.elapsed() / runs;
    eprintln!("generate: 50 clips, 60 s target: {per_run:?} per run");
    let budget_ms = if cfg!(debug_assertions) { 1_000 } else { 50 };
    assert!(
        per_run.as_millis() < budget_ms,
        "{per_run:?} exceeds {budget_ms} ms"
    );
}
