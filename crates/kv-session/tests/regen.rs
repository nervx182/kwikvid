//! Regenerate, re-roll and auto-regenerate through a `Session`, with a
//! synthetic music analysis (steady beats) and fake media.

use kv_autoedit::MusicAnalysis;
use kv_core::*;
use kv_session::regen::{AnalysisSource, NotReady, RegenError};
use kv_session::Session;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

const S: SectionId = Project::DEFAULT_SECTION;
const SEC: i64 = 1_000_000;
const SONG: &str = "music/song.mp3";

fn secs(s: i64) -> Micros {
    Micros(s * SEC)
}

/// 120 bpm, downbeat every 4 beats, flat energy.
fn music(len: i64) -> MusicAnalysis {
    let beats: Vec<Micros> = (0..len * 2).map(|i| Micros(i * SEC / 2)).collect();
    MusicAnalysis {
        duration: secs(len),
        tempo_bpm: 120.0,
        downbeats: beats.iter().step_by(4).copied().collect(),
        beats,
        onsets: Vec::new(),
        energy: vec![0.3; (len * 10) as usize],
    }
}

fn source(len: i64) -> HashMap<String, Arc<MusicAnalysis>> {
    HashMap::from([(SONG.to_string(), Arc::new(music(len)))])
}

fn root() -> PathBuf {
    std::env::temp_dir()
}

fn add_clip(id: u64) -> Command {
    Command::AddMedia {
        item: MediaItem {
            id: MediaId(id),
            path: format!("clips/GX{id:04}.MP4"),
            duration: secs(10),
            kind: MediaKind::Video,
            hilights: vec![secs(2), secs(7)],
            clip_audio: false,
        },
    }
}

fn set_music() -> Command {
    Command::SetMusic {
        section: S,
        music: Some(MusicRef {
            path: SONG.into(),
            duration: secs(40),
        }),
    }
}

fn fixed(s: i64) -> Command {
    Command::SetTargetLength {
        section: S,
        target: TargetLength::Fixed { length: secs(s) },
    }
}

fn range(id: u64, start: i64, end: i64) -> RangeRef {
    RangeRef {
        media_id: MediaId(id),
        start: secs(start),
        end: secs(end),
    }
}

fn edit(s: &Session) -> &Edl {
    &s.project().section(S).unwrap().edit
}

fn covers(edl: &Edl, r: &RangeRef) -> bool {
    edl.segments
        .iter()
        .any(|g| g.media_id == r.media_id && g.src_in <= r.start && g.src_out >= r.end)
}

/// A session with `clips` clips, music and a 15 s target, edit generated
/// (by plain `apply` plus an explicit regenerate).
fn ready(clips: u64, src: &dyn AnalysisSource) -> Session {
    let mut s = Session::new(root());
    for id in 1..=clips {
        s.apply(add_clip(id)).unwrap();
    }
    s.apply(set_music()).unwrap();
    s.apply(fixed(15)).unwrap();
    s.regenerate(S, src).unwrap();
    s
}

#[test]
fn regenerate_is_deterministic_per_seed() {
    let src = source(40);
    let a = ready(4, &src);
    let b = ready(4, &src);
    assert!(!edit(&a).segments.is_empty());
    assert_eq!(edit(&a), edit(&b));
    let end = edit(&a)
        .segments
        .last()
        .map(|g| g.timeline_start.0 + g.timeline_len().0);
    assert_eq!(end, Some(15 * SEC), "edit ends on the target beat");

    // Regenerating again with nothing changed is a no-op without an undo step.
    let mut a = a;
    let rev = a.revision();
    let snap = a.regenerate(S, &src).unwrap();
    assert_eq!(snap.snapshot.revision, rev);
    assert!(!snap.manual_edits_replaced);
}

#[test]
fn regenerate_reports_not_ready() {
    let src = source(40);
    let mut s = Session::new(root());
    assert_eq!(
        s.regenerate(S, &src),
        Err(RegenError::NotReady(NotReady::NoMedia))
    );
    s.apply(add_clip(1)).unwrap();
    let e = s.regenerate(S, &src).unwrap_err();
    assert_eq!(e, RegenError::NotReady(NotReady::NoMusic));
    assert_eq!(e.to_string(), "Add music to create an edit.");
    s.apply(set_music()).unwrap();
    let empty: HashMap<String, Arc<MusicAnalysis>> = HashMap::new();
    assert_eq!(
        s.regenerate(S, &empty),
        Err(RegenError::NotReady(NotReady::AnalysisPending))
    );
    assert!(edit(&s).segments.is_empty());
}

#[test]
fn full_song_target_uses_music_duration() {
    let src = source(20);
    let mut s = Session::new(root());
    for id in 1..=4 {
        s.apply(add_clip(id)).unwrap();
    }
    s.apply(set_music()).unwrap();
    s.regenerate(S, &src).unwrap();
    let last = edit(&s).segments.last().unwrap();
    assert_eq!(last.timeline_start.0 + last.timeline_len().0, 20 * SEC);
}

#[test]
fn reroll_changes_the_edit_but_keeps_pins() {
    let src = source(40);
    let mut s = ready(4, &src);
    let pin = range(3, 4, 6);
    s.apply_and_regenerate(
        Command::PinRange {
            section: S,
            range: pin,
        },
        &src,
    )
    .unwrap();
    assert!(covers(edit(&s), &pin));
    for round in 0..5 {
        let before = edit(&s).clone();
        let seed = s.project().section(S).unwrap().seed;
        let snap = s.reroll(S, &src).unwrap();
        assert!(!snap.manual_edits_replaced);
        let after = edit(&s);
        assert_ne!(*after, before, "round {round}");
        assert!(covers(after, &pin), "round {round}: pin dropped");
        assert!(s.project().section(S).unwrap().seed > seed);
    }

    // One undo restores both the seed and the edit.
    let before = (s.project().section(S).unwrap().seed, edit(&s).clone());
    s.reroll(S, &src).unwrap();
    s.undo().unwrap();
    assert_eq!(
        (s.project().section(S).unwrap().seed, edit(&s).clone()),
        before
    );
}

#[test]
fn reroll_is_deterministic() {
    let src = source(40);
    let mut a = ready(4, &src);
    let mut b = ready(4, &src);
    a.reroll(S, &src).unwrap();
    b.reroll(S, &src).unwrap();
    assert_eq!(a.project(), b.project());
    assert_eq!(a.project().section(S).unwrap().seed, 1);
}

/// Each trigger, applied to a ready session, returns a regenerated edit that
/// matches an explicit regenerate of the resulting constraints.
#[test]
fn each_trigger_regenerates() {
    let src = source(40);
    let pin = range(2, 1, 3);
    let ex = range(1, 0, 10);
    let cases: Vec<(&str, Vec<Command>, Command)> = vec![
        ("set_music", vec![], {
            let mut c = set_music();
            if let Command::SetMusic { music: Some(m), .. } = &mut c {
                m.duration = secs(39);
            }
            c
        }),
        ("set_target_length", vec![], fixed(10)),
        (
            "pin_range",
            vec![],
            Command::PinRange {
                section: S,
                range: pin,
            },
        ),
        (
            "unpin_range",
            vec![Command::PinRange {
                section: S,
                range: pin,
            }],
            Command::UnpinRange {
                section: S,
                range: pin,
            },
        ),
        (
            "exclude_range",
            vec![],
            Command::ExcludeRange {
                section: S,
                range: ex,
            },
        ),
        (
            "unexclude_range",
            vec![Command::ExcludeRange {
                section: S,
                range: ex,
            }],
            Command::UnexcludeRange {
                section: S,
                range: ex,
            },
        ),
        (
            "set_style",
            vec![],
            Command::SetStyle {
                section: S,
                style: StylePreset::Smooth,
            },
        ),
        ("add_media", vec![], add_clip(9)),
        (
            "remove_media",
            vec![],
            Command::RemoveMedia {
                media_id: MediaId(4),
            },
        ),
    ];
    for (name, setup, cmd) in cases {
        let mut s = ready(4, &src);
        for c in setup {
            // Setup through plain apply leaves the edit stale on purpose.
            s.apply(c).unwrap();
        }
        let before = s.project().clone();
        let snap = s.apply_and_regenerate(cmd, &src).unwrap();
        assert_eq!(snap.regen_error, None, "{name}");
        assert!(!snap.manual_edits_replaced, "{name}");
        let got = edit(&s).clone();
        assert!(!got.segments.is_empty(), "{name}");
        // An explicit regenerate now changes nothing: the edit is fresh.
        let rev = s.revision();
        s.regenerate(S, &src).unwrap();
        assert_eq!(s.revision(), rev, "{name}: edit was not regenerated");
        assert_eq!(*edit(&s), got, "{name}");
        // One undo step brings back the state before the action.
        s.undo().unwrap();
        assert_eq!(*s.project(), before, "{name}");
    }
}

#[test]
fn trigger_changes_edl_for_constraints() {
    let src = source(40);
    let mut s = ready(4, &src);
    let before = edit(&s).clone();
    s.apply_and_regenerate(fixed(10), &src).unwrap();
    assert_ne!(*edit(&s), before);
    let ex = range(1, 0, 10);
    s.apply_and_regenerate(
        Command::ExcludeRange {
            section: S,
            range: ex,
        },
        &src,
    )
    .unwrap();
    assert!(edit(&s).segments.iter().all(|g| g.media_id != MediaId(1)));
}

#[test]
fn manual_tweaks_do_not_regenerate_and_are_reported_when_replaced() {
    let src = source(40);
    let mut s = ready(4, &src);
    let generated = edit(&s).clone();
    let snap = s
        .apply_and_regenerate(
            Command::MoveSegment {
                section: S,
                from: 0,
                to: 1,
            },
            &src,
        )
        .unwrap();
    assert!(!snap.manual_edits_replaced);
    let tweaked = edit(&s).clone();
    assert_ne!(tweaked, generated, "the tweak stays");

    let cut_at = tweaked.segments[1].timeline_start.0 + SEC / 2;
    s.apply_and_regenerate(
        Command::MoveCut {
            section: S,
            cut: 1,
            to: Micros(cut_at),
        },
        &src,
    )
    .unwrap();
    let tweaked = edit(&s).clone();

    // A pin regenerates and replaces the tweaks.
    let pin = range(2, 1, 3);
    let snap = s
        .apply_and_regenerate(
            Command::PinRange {
                section: S,
                range: pin,
            },
            &src,
        )
        .unwrap();
    assert!(snap.manual_edits_replaced);
    assert_ne!(*edit(&s), tweaked);
    assert!(covers(edit(&s), &pin));

    // Snapshot JSON carries the flag next to the snapshot keys.
    let json = serde_json::to_value(&snap).unwrap();
    assert_eq!(json["manual_edits_replaced"], true);
    assert_eq!(json["regen_error"], serde_json::Value::Null);
    assert!(json["revision"].is_u64() && json["project"].is_object());

    // Undo brings back the tweaked edit and removes the pin in one step.
    s.undo().unwrap();
    assert_eq!(*edit(&s), tweaked);
    assert!(s.project().section(S).unwrap().pins.is_empty());

    // Re-roll and explicit regenerate also report replaced tweaks.
    let snap = s.reroll(S, &src).unwrap();
    assert!(snap.manual_edits_replaced);
    s.undo().unwrap();
    let snap = s.regenerate(S, &src).unwrap();
    assert!(snap.manual_edits_replaced);
}

#[test]
fn failed_regeneration_applies_the_command_and_reports_why() {
    let src = source(40);
    let mut s = ready(4, &src);
    let before = edit(&s).clone();
    // Two 4 s pins together do not fit a 5 s target.
    s.apply_and_regenerate(fixed(5), &src).unwrap();
    let five = edit(&s).clone();
    assert_ne!(five, before);
    let snap = s
        .apply_and_regenerate(
            Command::PinRange {
                section: S,
                range: range(1, 0, 4),
            },
            &src,
        )
        .unwrap();
    assert_eq!(snap.regen_error, None);
    let snap = s
        .apply_and_regenerate(
            Command::PinRange {
                section: S,
                range: range(2, 0, 4),
            },
            &src,
        )
        .unwrap();
    let msg = snap.regen_error.expect("pins exceed the target");
    assert!(msg.contains("longer than the target length"), "{msg}");
    assert_eq!(s.project().section(S).unwrap().pins.len(), 2);

    // A rejected command changes nothing.
    let rev = s.revision();
    assert!(s
        .apply_and_regenerate(
            Command::PinRange {
                section: S,
                range: range(2, 0, 4),
            },
            &src,
        )
        .is_err());
    assert_eq!(s.revision(), rev);

    // Adding media before music: applied, with a "not ready" notice.
    let mut s = Session::new(root());
    let snap = s.apply_and_regenerate(add_clip(1), &src).unwrap();
    assert_eq!(
        snap.regen_error.as_deref(),
        Some("Add music to create an edit.")
    );
    assert_eq!(s.project().media.len(), 1);
    for id in 2..=4 {
        s.apply_and_regenerate(add_clip(id), &src).unwrap();
    }
    // Adding music then creates the edit (full song: 40 s of 4 x 10 s clips).
    let snap = s.apply_and_regenerate(set_music(), &src).unwrap();
    assert_eq!(snap.regen_error, None);
    assert!(!edit(&s).segments.is_empty());
}

/// Roadmap target: regenerate for 50 clips under 300 ms in release. Logged,
/// not asserted (the gate runs debug builds on shared runners).
#[test]
fn regenerate_50_clips_timing() {
    let src = source(60);
    let mut s = Session::new(root());
    for id in 1..=50 {
        s.apply(add_clip(id)).unwrap();
    }
    s.apply(set_music()).unwrap();
    let t = Instant::now();
    let snap = s.apply_and_regenerate(fixed(60), &src).unwrap();
    let took = t.elapsed();
    assert_eq!(snap.regen_error, None);
    eprintln!(
        "regenerate 50 clips, 60 s target: {took:?} ({} segments, {} build)",
        edit(&s).segments.len(),
        if cfg!(debug_assertions) {
            "debug"
        } else {
            "release"
        }
    );
}
