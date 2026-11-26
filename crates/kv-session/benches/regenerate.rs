//! Auto-edit regenerate latency for 50 clips (issue #147), measured against
//! the Roadmap I target "auto-edit regenerate < 300 ms for 50 clips"
//! (`docs/roadmap.md`).
//!
//! The fixture is built in memory, no files: 50 video `MediaItem`s of 20 to
//! 80 s (two thirds with HiLights, like `fifty_clips_sixty_seconds_is_fast`
//! in `crates/kv-autoedit/tests/scenarios.rs`), a synthetic 180 s
//! `MusicAnalysis` (128 BPM beats, a downbeat every 4 beats, off-beat onsets,
//! a varying energy curve) and a fixed 60 s target. Cases:
//!
//! - `generate`: `kv_autoedit::generate` alone on the session's input;
//! - `apply-pin`: `Session::apply_and_regenerate(PinRange ..)` on a session
//!   that already has an edit, pinning a range the edit does not cover. This
//!   includes the second generate that detects manual tweaks (ADR 0010);
//! - `apply-pin-ipc`: the same plus `serde_json::to_string(&RegenSnapshot)`,
//!   the payload kv-app sends over IPC. This is the number compared with the
//!   300 ms target (`TARGET regenerate-50-clips`);
//! - `reroll`: `Session::reroll`.
//!
//! The session is cloned before every measured call (outside the timing), so
//! each call starts from the same state.
//!
//! # Output
//!
//! Besides criterion's own lines, each case prints one machine-readable line
//!
//! ```text
//! TARGET <name> <measured_ms> <target_ms>
//! ```
//!
//! where `<measured_ms>` is the median of every individual call (warm-up
//! included) and `<target_ms>` the roadmap budget. The regression
//! `benchmarks` job turns these lines into its "Performance targets" table
//! (see AGENTS.md, "Full regression suite").
//!
//! Runs only in the regression `benchmarks` job:
//! `cargo bench -p kv-session --bench regenerate`.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use criterion::{criterion_group, criterion_main, Criterion};
use kv_audio::MusicAnalysis;
use kv_core::{
    Command, MediaId, MediaItem, MediaKind, Micros, MusicRef, Project, RangeRef, SectionId,
    TargetLength,
};
use kv_session::regen::{edit_input, AnalysisSource};
use kv_session::Session;

/// The Roadmap I regenerate budget.
const TARGET_MS: f64 = 300.0;
const S: SectionId = Project::DEFAULT_SECTION;
const SONG: &str = "music/song.mp3";
const CLIPS: u64 = 50;

fn secs(s: f64) -> Micros {
    Micros::from_secs_f64(s)
}

/// 180 s at 128 BPM: beats, downbeats every 4 beats, an onset between beats,
/// energy rising and falling over 100 ms blocks.
fn music() -> MusicAnalysis {
    let duration = 180.0;
    let beat = 60.0 / 128.0;
    let beats: Vec<Micros> = (0..)
        .map(|i| f64::from(i) * beat + 0.25)
        .take_while(|&t| t < duration)
        .map(secs)
        .collect();
    let onsets = beats
        .iter()
        .flat_map(|b| [*b, Micros(b.0 + secs(beat / 2.0).0)])
        .filter(|t| t.0 < secs(duration).0)
        .collect();
    let energy = (0..(duration * 10.0) as usize)
        .map(|k| 0.2 + 0.15 * ((k as f32) / 80.0).sin().abs() + ((k % 7) as f32) * 0.01)
        .collect();
    MusicAnalysis {
        duration: secs(duration),
        tempo_bpm: 128.0,
        downbeats: beats.iter().step_by(4).copied().collect(),
        beats,
        onsets,
        energy,
    }
}

fn clip(i: u64) -> MediaItem {
    let len = 20.0 + (i % 7) as f64 * 10.0;
    MediaItem {
        id: MediaId(i + 1),
        path: format!("clips/GX01{:04}.MP4", i + 1),
        duration: secs(len),
        kind: MediaKind::Video,
        hilights: (0..i % 3)
            .map(|k| secs(len * (k as f64 + 1.0) / 4.0))
            .collect(),
        clip_audio: false,
        chapters: Vec::new(),
    }
}

/// A session with 50 clips, music and a 60 s target, edit generated.
fn session(src: &dyn AnalysisSource) -> Session {
    let mut s = Session::new(std::env::temp_dir());
    for i in 0..CLIPS {
        s.apply(Command::AddMedia { item: clip(i) }).unwrap();
    }
    s.apply(Command::SetMusic {
        section: S,
        music: Some(MusicRef {
            path: SONG.into(),
            duration: secs(180.0),
        }),
    })
    .unwrap();
    s.apply(Command::SetTargetLength {
        section: S,
        target: TargetLength::Fixed { length: secs(60.0) },
    })
    .unwrap();
    s.regenerate(S, src).unwrap();
    s
}

/// A 2 s range the current edit does not touch, so pinning it changes the
/// edit (and runs the manual-tweak check).
fn uncovered_range(s: &Session) -> RangeRef {
    let edl = &s.project().section(S).unwrap().edit;
    for m in &s.project().media {
        for start in (0..m.duration.0 / 1_000_000 - 2).step_by(2) {
            let r = RangeRef {
                media_id: m.id,
                start: secs(start as f64),
                end: secs(start as f64 + 2.0),
            };
            let touched = edl
                .segments
                .iter()
                .any(|g| g.media_id == r.media_id && g.src_in < r.end && r.start < g.src_out);
            if !touched {
                return r;
            }
        }
    }
    panic!("the edit covers every clip completely");
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1e3
}

/// Measure `call` (timed alone; `setup` runs before each call, untimed) and
/// print its `TARGET` line.
fn bench_case<T>(
    c: &mut Criterion,
    name: &str,
    target_name: &str,
    mut setup: impl FnMut() -> T,
    mut call: impl FnMut(T),
) {
    let mut samples: Vec<Duration> = Vec::new();
    c.bench_function(&format!("regenerate-50-clips/{name}"), |b| {
        b.iter_custom(|iters| {
            let mut total = Duration::ZERO;
            for _ in 0..iters {
                let input = setup();
                let started = Instant::now();
                call(input);
                let took = started.elapsed();
                samples.push(took);
                total += took;
            }
            total
        })
    });
    samples.sort_unstable();
    let median = samples[samples.len() / 2];
    println!("TARGET {target_name} {:.2} {TARGET_MS:.0}", ms(median));
}

fn regenerate(c: &mut Criterion) {
    let src: HashMap<String, Arc<MusicAnalysis>> =
        HashMap::from([(SONG.to_string(), Arc::new(music()))]);
    let base = session(&src);
    let pin = Command::PinRange {
        section: S,
        range: uncovered_range(&base),
    };
    eprintln!(
        "regenerate-50-clips: {} segments in the edit, pinning {:?}",
        base.project().section(S).unwrap().edit.segments.len(),
        pin
    );

    let input = edit_input(base.project(), S, Some(&src[SONG]), |_| None).unwrap();
    bench_case(
        c,
        "generate",
        "regenerate-50-clips-generate-only",
        || (),
        |()| {
            std::hint::black_box(kv_autoedit::generate(&input).unwrap());
        },
    );

    bench_case(
        c,
        "apply-pin",
        "regenerate-50-clips-apply-pin",
        || base.clone(),
        |mut s| {
            let snap = s.apply_and_regenerate(pin.clone(), &src).unwrap();
            assert_eq!(snap.regen_error, None);
            std::hint::black_box(snap);
        },
    );

    bench_case(
        c,
        "apply-pin-ipc",
        "regenerate-50-clips",
        || base.clone(),
        |mut s| {
            let snap = s.apply_and_regenerate(pin.clone(), &src).unwrap();
            std::hint::black_box(serde_json::to_string(&snap).unwrap());
        },
    );

    bench_case(
        c,
        "reroll",
        "regenerate-50-clips-reroll",
        || base.clone(),
        |mut s| {
            std::hint::black_box(s.reroll(S, &src).unwrap());
        },
    );
}

criterion_group!(benches, regenerate);
criterion_main!(benches);
