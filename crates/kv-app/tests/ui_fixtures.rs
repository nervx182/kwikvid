//! Snapshot fixtures for the Playwright UI tests (`ui-tests/fixtures/*.json`,
//! ADR 0011).
//!
//! The mock backend in `ui-tests/mock-api.js` serves these files as the
//! state the UI renders, so they must match what `kv_session::Snapshot`
//! really serialises to. This test rebuilds each fixture from a real session
//! and fails if the checked-in file differs. After changing `Snapshot` or
//! `Project`, regenerate them with:
//!
//! ```sh
//! KV_UPDATE_UI_FIXTURES=1 cargo test -p kv-app --test ui_fixtures
//! ```
//!
//! To add a fixture, add an entry to [`fixtures`] (a plain [`Snapshot`]) or
//! [`other_fixtures`] (anything else the mock serves: a
//! [`RegenSnapshot`], a `get_beats` answer).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use kv_audio::MusicAnalysis;
use kv_core::{
    ChapterRef, Command, Edl, MediaId, MediaItem, MediaKind, Micros, MusicRef, Project, RangeRef,
    Segment, Speed, StylePreset, TargetLength, TitleCard, Transition,
};
use kv_session::regen::RegenSnapshot;
use kv_session::{ImportReport, Session, Snapshot};

/// A fixed media root so the fixtures do not depend on the machine.
const MEDIA_ROOT: &str = "/home/ana";

fn session() -> Session {
    Session::new(PathBuf::from(MEDIA_ROOT))
}

fn set_seed(seed: u64) -> Command {
    Command::SetSeed {
        section: Project::DEFAULT_SECTION,
        seed,
    }
}

const S: kv_core::SectionId = Project::DEFAULT_SECTION;
const SEC: i64 = 1_000_000;

fn ms(v: i64) -> Micros {
    Micros(v * 1000)
}

fn clip(id: u64, name: &str, secs: i64) -> Command {
    Command::AddMedia {
        item: MediaItem {
            id: MediaId(id),
            path: format!("DCIM/100GOPRO/{name}"),
            duration: Micros(secs * SEC),
            kind: MediaKind::Video,
            hilights: Vec::new(),
            clip_audio: false,
            chapters: Vec::new(),
        },
    }
}

fn project_clip(id: u64, path: &str) -> Command {
    Command::AddMedia {
        item: MediaItem {
            id: MediaId(id),
            path: path.into(),
            duration: Micros(12_000_000),
            kind: MediaKind::Video,
            hilights: vec![],
            clip_audio: false,
            chapters: vec![],
        },
    }
}

/// Where the project-file fixtures pretend to live (the real files are
/// written to a temporary folder).
const PROJECT_DIR: &str = "/home/ana/Movies/trip";

fn song() -> MusicRef {
    MusicRef {
        path: "Music/song.mp3".into(),
        duration: Micros(8 * SEC),
    }
}

/// 120 bpm over 8 s: a beat every 0.5 s, a downbeat every 4 beats.
fn song_analysis() -> MusicAnalysis {
    let beats: Vec<Micros> = (0..16).map(|i| ms(i * 500)).collect();
    MusicAnalysis {
        duration: Micros(8 * SEC),
        tempo_bpm: 120.0,
        downbeats: beats.iter().step_by(4).copied().collect(),
        beats,
        onsets: Vec::new(),
        energy: Vec::new(),
    }
}

fn seg(media: u64, src_in: i64, src_out: i64, transition: Transition, speed: u32) -> Segment {
    Segment {
        media_id: MediaId(media),
        src_in: ms(src_in),
        src_out: ms(src_out),
        timeline_start: Micros(0),
        transition,
        speed: Speed(speed),
    }
}

/// A session with three clips (10 s, 6 s, 8 s) and nothing else.
fn session_with_clips() -> Session {
    let mut s = session();
    s.apply(clip(1, "GX010001.MP4", 10)).expect("clip 1");
    s.apply(clip(2, "GX010002.MP4", 6)).expect("clip 2");
    s.apply(clip(3, "GX010003.MP4", 8)).expect("clip 3");
    s
}

/// The timeline fixture: an 8 s edit of four segments over an 8 s song,
/// laid out by `Edl::repack`:
///
/// | # | clip | source      | transition   | speed | timeline    |
/// |---|------|-------------|--------------|-------|-------------|
/// | 0 | 1    | 1.0-3.0 s   | cut          | 100   | 0.0-2.0 s   |
/// | 1 | 2    | 0.0-2.5 s   | 0.5 s fade   | 100   | 1.5-4.0 s   |
/// | 2 | 3    | 2.0-3.0 s   | cut          | 50    | 4.0-6.0 s   |
/// | 3 | 1    | 6.0-8.0 s   | cut          | 100   | 6.0-8.0 s   |
///
/// Clip 3's 2.2-2.8 s is pinned, so segment 2 covers a pin.
fn timeline_edit() -> Snapshot {
    timeline_session().snapshot()
}

/// The session behind [`timeline_edit`].
fn timeline_session() -> Session {
    let mut s = session_with_clips();
    s.apply(Command::SetMusic {
        section: S,
        music: Some(song()),
    })
    .expect("music");
    s.apply(Command::SetTargetLength {
        section: S,
        target: TargetLength::Fixed {
            length: Micros(8 * SEC),
        },
    })
    .expect("target");
    s.apply(Command::PinRange {
        section: S,
        range: RangeRef {
            media_id: MediaId(3),
            start: ms(2200),
            end: ms(2800),
        },
    })
    .expect("pin");
    let mut edl = Edl {
        segments: vec![
            seg(1, 1000, 3000, Transition::Cut, 100),
            seg(2, 0, 2500, Transition::Crossfade { duration: ms(500) }, 100),
            seg(3, 2000, 3000, Transition::Cut, 50),
            seg(1, 6000, 8000, Transition::Cut, 100),
        ],
    };
    edl.repack();
    s.apply(Command::ReplaceEdl { section: S, edl })
        .expect("edl");
    s
}

/// The manual tweak fixtures (`ui/js/tweaks.js`), each one command applied
/// to the timeline fixture (with its song analysed):
///
/// - `tweaks-moved`: segment 0 moved to index 2 (`MoveSegment`).
/// - `tweaks-nudged`: cut 2 (4.0 s) nudged one beat later, to 4.5 s.
/// - `tweaks-clip-audio`: clip 1's audio on (segments 0 and 3).
/// - `tweaks-volume`: music volume 40%.
fn tweaks_fixtures() -> Vec<(&'static str, Snapshot)> {
    let mut base = timeline_session();
    base.set_music_analysis(song().path, song_analysis());
    let moved = base
        .clone()
        .apply(Command::MoveSegment {
            section: S,
            from: 0,
            to: 2,
        })
        .expect("move segment");
    let nudged = base.clone().nudge_cut(S, 2, 1).expect("nudge");
    assert_eq!(
        nudged.project.sections[0].edit.segments[2].timeline_start,
        ms(4500)
    );
    let audio = base
        .clone()
        .apply(Command::SetClipAudio {
            media_id: MediaId(1),
            enabled: true,
        })
        .expect("clip audio");
    let volume = base
        .apply(Command::SetMusicVolume { percent: 40 })
        .expect("volume");
    vec![
        ("tweaks-moved", moved),
        ("tweaks-nudged", nudged),
        ("tweaks-clip-audio", audio),
        ("tweaks-volume", volume),
    ]
}

fn fixed(secs: i64) -> Command {
    Command::SetTargetLength {
        section: S,
        target: TargetLength::Fixed {
            length: Micros(secs * SEC),
        },
    }
}

/// A 40 s song at 120 bpm, analysed.
fn long_song() -> (MusicRef, MusicAnalysis) {
    let music = MusicRef {
        path: "Music/long-song.mp3".into(),
        duration: Micros(40 * SEC),
    };
    let beats: Vec<Micros> = (0..80).map(|i| ms(i * 500)).collect();
    let analysis = MusicAnalysis {
        duration: Micros(40 * SEC),
        tempo_bpm: 120.0,
        downbeats: beats.iter().step_by(4).copied().collect(),
        beats,
        onsets: Vec::new(),
        energy: vec![0.3; 400],
    };
    (music, analysis)
}

/// The auto-edit control fixtures (`ui/js/autoedit.js`), each a
/// [`RegenSnapshot`] as the `apply` / `reroll` commands return it:
///
/// - `autoedit-ready`: four clips (44 s), the 40 s song analysed, target
///   set to 15 s; the edit was generated with seed 0.
/// - `autoedit-rerolled`: `autoedit-ready` re-rolled (seed 1).
/// - `autoedit-manual-replaced`: `autoedit-ready` with two segments swapped
///   by hand, then the target set to 30 s: `manual_edits_replaced`.
/// - `autoedit-pins-too-long`: a 20 s pin, then the target set to 15 s: the
///   target applies but the edit cannot be rebuilt (`regen_error`).
fn autoedit_fixtures() -> Vec<(&'static str, String)> {
    let (music, analysis) = long_song();
    let mut s = session_with_clips();
    s.apply(clip(4, "GX010004.MP4", 20)).expect("clip 4");
    s.apply(Command::SetMusic {
        section: S,
        music: Some(music.clone()),
    })
    .expect("music");
    s.set_music_analysis(music.path.clone(), analysis);
    let base = s.clone();
    let source = s.analyses();

    let ready = s.apply_and_regenerate(fixed(15), &source).expect("15 s");
    assert!(
        ready.regen_error.is_none() && !ready.snapshot.project.sections[0].edit.segments.is_empty()
    );

    let mut r = s.clone();
    let rerolled = r.reroll(S, &source).expect("reroll");

    let mut m = s.clone();
    m.apply(Command::MoveSegment {
        section: S,
        from: 0,
        to: 1,
    })
    .expect("manual tweak");
    let replaced = m.apply_and_regenerate(fixed(30), &source).expect("30 s");
    assert!(
        replaced.manual_edits_replaced,
        "expected manual edits replaced"
    );

    let mut p = base;
    p.apply(Command::PinRange {
        section: S,
        range: RangeRef {
            media_id: MediaId(4),
            start: Micros(0),
            end: Micros(20 * SEC),
        },
    })
    .expect("pin");
    let too_long = p.apply_and_regenerate(fixed(15), &source).expect("15 s");
    assert!(too_long.regen_error.is_some(), "expected a regen error");

    vec![
        ("autoedit-ready", render(&ready)),
        ("autoedit-rerolled", render(&rerolled)),
        ("autoedit-manual-replaced", render(&replaced)),
        ("autoedit-pins-too-long", render(&too_long)),
    ]
}

/// - `styles-smooth`: `autoedit-ready` with the Smooth preset chosen
///   (`SetStyle`, applied with a regenerate), so its edit has crossfades.
/// - `styles-titled`: `styles-smooth` with the title "Lake day" for 3 s.
fn styles_fixtures() -> Vec<(&'static str, String)> {
    let (music, analysis) = long_song();
    let mut s = session_with_clips();
    s.apply(clip(4, "GX010004.MP4", 20)).expect("clip 4");
    s.apply(Command::SetMusic {
        section: S,
        music: Some(music.clone()),
    })
    .expect("music");
    s.set_music_analysis(music.path.clone(), analysis);
    let source = s.analyses();
    s.apply_and_regenerate(fixed(15), &source).expect("15 s");
    let smooth = s
        .apply_and_regenerate(
            Command::SetStyle {
                section: S,
                style: StylePreset::Smooth,
            },
            &source,
        )
        .expect("smooth");
    assert!(smooth.regen_error.is_none(), "{:?}", smooth.regen_error);
    assert!(
        s.project().sections[0]
            .edit
            .segments
            .iter()
            .any(|g| matches!(g.transition, Transition::Crossfade { .. })),
        "Smooth gives crossfades"
    );
    let titled = s
        .apply_and_regenerate(
            Command::SetTitleCard {
                section: S,
                title: Some(TitleCard {
                    text: "Lake day".into(),
                    duration: Micros(3 * SEC),
                }),
            },
            &source,
        )
        .expect("title");
    vec![
        ("styles-smooth", render(&smooth)),
        ("styles-titled", render(&titled)),
    ]
}

/// The library fixture: a 75 s clip with HiLights at 15 s and 60 s, a
/// 10 s clip without any, and a chaptered 40 s recording (two 20 s
/// chapters) with a HiLight at 30 s.
fn library_session() -> Session {
    let mut s = session();
    let item = |id: u64, name: &str, secs: i64, hilights: &[i64]| MediaItem {
        id: MediaId(id),
        path: format!("DCIM/100GOPRO/{name}"),
        duration: Micros(secs * SEC),
        kind: MediaKind::Video,
        hilights: hilights.iter().map(|h| Micros(h * SEC)).collect(),
        clip_audio: false,
        chapters: Vec::new(),
    };
    let mut chaptered = item(3, "GX010042.MP4", 40, &[30]);
    chaptered.chapters = ["GX010042.MP4", "GX020042.MP4"]
        .iter()
        .map(|n| ChapterRef {
            path: format!("DCIM/100GOPRO/{n}"),
            duration: Micros(20 * SEC),
        })
        .collect();
    for item in [
        item(1, "GX010001.MP4", 75, &[15, 60]),
        item(2, "GX010002.MP4", 10, &[]),
        chaptered,
    ] {
        s.apply(Command::AddMedia { item }).expect("library item");
    }
    s
}

fn range(media: u64, start_ms: i64, end_ms: i64) -> RangeRef {
    RangeRef {
        media_id: MediaId(media),
        start: ms(start_ms),
        end: ms(end_ms),
    }
}

/// The clip inspector fixtures (`ui/js/inspector.js`), all on top of the
/// library fixture:
///
/// - `inspector-pinned`: clip 1's 10-15 s pinned.
/// - `inspector-constraints`: that pin, clip 1's 40-50 s excluded and the
///   whole of clip 2 (0-10 s) excluded.
/// - `inspector-pins-too-long`: the 40 s song analysed and a 15 s target,
///   then clip 1's 0-20 s pinned through `apply`: the pin applies but the
///   edit cannot be rebuilt (`regen_error`).
fn inspector_fixtures() -> Vec<(&'static str, String)> {
    let mut s = library_session();
    let pinned = s
        .apply(Command::PinRange {
            section: S,
            range: range(1, 10_000, 15_000),
        })
        .expect("pin");
    s.apply(Command::ExcludeRange {
        section: S,
        range: range(1, 40_000, 50_000),
    })
    .expect("exclude range");
    let constraints = s
        .apply(Command::ExcludeRange {
            section: S,
            range: range(2, 0, 10_000),
        })
        .expect("exclude clip");

    let (music, analysis) = long_song();
    let mut s = library_session();
    s.apply(Command::SetMusic {
        section: S,
        music: Some(music.clone()),
    })
    .expect("music");
    s.set_music_analysis(music.path.clone(), analysis);
    s.apply(fixed(15)).expect("target");
    let source = s.analyses();
    let too_long = s
        .apply_and_regenerate(
            Command::PinRange {
                section: S,
                range: range(1, 0, 20_000),
            },
            &source,
        )
        .expect("pin");
    assert_eq!(
        too_long.regen_error.as_deref(),
        Some("A pinned moment is longer than the target length.")
    );

    vec![
        ("inspector-pinned", render(&pinned)),
        ("inspector-constraints", render(&constraints)),
        ("inspector-pins-too-long", render(&too_long)),
    ]
}

/// The answer of `import` for the library fixture, with one skipped photo.
fn import_report() -> ImportReport {
    ImportReport {
        added: vec![MediaId(1), MediaId(2), MediaId(3)],
        skipped: vec![(
            PathBuf::from("/home/ana/DCIM/100GOPRO/GOPR0007.JPG"),
            "Only MP4 and MOV videos can be imported for now.".into(),
        )],
    }
}

/// Fixtures that are not a plain [`Snapshot`], rendered.
fn other_fixtures() -> Vec<(&'static str, String)> {
    // Music set while its analysis is still running: the auto-regenerate
    // after `SetMusic` cannot run, so the snapshot carries `regen_error`.
    let mut s = session_with_clips();
    let no_analysis: HashMap<String, Arc<MusicAnalysis>> = HashMap::new();
    let pending = s
        .apply_and_regenerate(
            Command::SetMusic {
                section: S,
                music: Some(song()),
            },
            &no_analysis,
        )
        .expect("set music");
    assert!(pending.regen_error.is_some(), "expected a regen error");

    // What `get_beats` answers for the timeline fixture's song.
    let mut s = session();
    s.apply(Command::SetMusic {
        section: S,
        music: Some(song()),
    })
    .expect("music");
    s.set_music_analysis(song().path, song_analysis());
    let beats = beats_json(&s);

    vec![
        ("timeline-regen-error", render::<RegenSnapshot>(&pending)),
        ("beats-song", render(&beats)),
        ("import-report", render(&import_report())),
    ]
}

/// The `get_beats` answer for the default section, as the app builds it
/// (`src/timeline.rs`, which this integration test cannot import): the
/// music path plus the analysis' duration, tempo, beats and downbeats.
fn beats_json(s: &Session) -> serde_json::Value {
    let music = s.project().sections[0].music.clone().expect("music");
    let a = s.music_analysis(&music).expect("analysis");
    serde_json::json!({
        "music": music.path,
        "duration": a.duration,
        "tempo_bpm": a.tempo_bpm,
        "beats": a.beats,
        "downbeats": a.downbeats,
    })
}

/// Every fixture: file name (without `.json`) and the snapshot it holds.
fn fixtures() -> Vec<(&'static str, Snapshot)> {
    // A fresh untitled project: nothing to undo or redo.
    let initial = session().snapshot();

    // One applied command: undo enabled, redo disabled, unsaved.
    let mut s = session();
    let edited = s.apply(set_seed(7)).expect("apply");

    // ...then undone: undo disabled, redo enabled.
    let undone = s.undo().expect("undo");

    // ...then redone: back to `edited` but at a newer revision.
    let redone = s.redo().expect("redo");

    // Two commands, one undone: both enabled.
    let mut s = session();
    s.apply(set_seed(7)).expect("apply");
    s.apply(set_seed(8)).expect("apply");
    let both = s.undo().expect("undo");

    // `edited`, then saved as trip.kwikvid: clean, revision bumped.
    let dir = tempfile::tempdir().expect("temp dir");
    let mut s = session();
    s.apply(set_seed(7)).expect("apply");
    let mut saved = s
        .save_as(&dir.path().join("saved.kwikvid"))
        .expect("save as");
    saved.media_root = PathBuf::from(PROJECT_DIR);

    // A project with three clips (none on disk), opened in place of
    // `edited` (revision 1): clean, no history, revision 2.
    let mut p = Session::new(dir.path().to_path_buf());
    for (id, name) in [(1, "GX010001"), (2, "GX010002"), (3, "GX010003")] {
        p.apply(project_clip(id, &format!("clips/{name}.MP4")))
            .expect("add clip");
    }
    let file = dir.path().join("trip.kwikvid");
    p.save_as(&file).expect("save project");
    let (mut o, missing) = Session::open(&file).expect("open");
    assert_eq!(missing.len(), 3);
    o.continue_revisions(edited.revision);
    let mut opened = o.snapshot();
    opened.media_root = PathBuf::from(PROJECT_DIR);

    // The library, then clip 2 removed.
    let mut s = library_session();
    let library = s.snapshot();
    let library_removed = s
        .apply(Command::RemoveMedia {
            media_id: MediaId(2),
        })
        .expect("remove");

    // The music picker: the song set (what the backend applies when its
    // analysis is done), then removed again.
    let mut s = session();
    let music_set = s
        .apply(Command::SetMusic {
            section: S,
            music: Some(song()),
        })
        .expect("set music");
    let music_removed = s
        .apply(Command::SetMusic {
            section: S,
            music: None,
        })
        .expect("remove music");

    vec![
        ("initial", initial),
        ("edited", edited),
        ("undone", undone),
        ("redone", redone),
        ("undo-redo", both),
        ("saved", saved),
        ("opened", opened),
        ("timeline-no-music", session_with_clips().snapshot()),
        ("timeline-edit", timeline_edit()),
        ("library", library),
        ("library-removed", library_removed),
        ("music-set", music_set),
        ("music-removed", music_removed),
    ]
    .into_iter()
    .chain(tweaks_fixtures())
    .collect()
}

/// Every fixture file: name and contents.
fn all_fixtures() -> Vec<(&'static str, String)> {
    let mut all: Vec<_> = fixtures()
        .into_iter()
        .map(|(n, s)| (n, render(&s)))
        .collect();
    all.extend(other_fixtures());
    all.extend(autoedit_fixtures());
    all.extend(inspector_fixtures());
    all.extend(styles_fixtures());
    all.extend(export_fixtures());
    all.extend(camera_fixtures());
    all
}

/// The camera import dialog's fixtures: `list_cameras` and
/// `list_camera_recordings` answers and a finished import's `kv://camera`
/// update. The recordings and the update are the app's `RecordingInfo` and
/// `CameraUpdate` (`src/camera.rs`, which this integration test cannot
/// import; a unit test there checks these keys): a `CameraRecording` plus
/// `chapters` and `already_copied` with `modified` in Unix milliseconds,
/// and `{ id, ...CopyEvent, report }`.
fn camera_fixtures() -> Vec<(&'static str, String)> {
    use kv_session::{CameraVolume, CopyEvent};
    let volume = CameraVolume {
        path: PathBuf::from("/Volumes/GOPRO"),
        name: "GOPRO".into(),
        is_gopro: true,
    };
    let dir = "/Volumes/GOPRO/DCIM/100GOPRO";
    // 2024-07-14 09:30 UTC and the next day.
    let recording = |names: &[&str], total_bytes: u64, modified: u64, copied: bool| {
        serde_json::json!({
            "files": names.iter().map(|n| format!("{dir}/{n}")).collect::<Vec<_>>(),
            "total_bytes": total_bytes,
            "modified": modified,
            "chapters": names.len(),
            "has_gaps": false,
            "already_copied": copied,
        })
    };
    let recordings = vec![
        recording(
            &["GX010042.MP4", "GX020042.MP4"],
            7_516_192_768,
            1_720_949_400_000,
            false,
        ),
        recording(&["GX010043.MP4"], 1_073_741_824, 1_720_949_400_000, true),
        recording(&["GX010044.MP4"], 536_870_912, 1_721_035_800_000, false),
    ];
    let copied = vec![
        PathBuf::from("/home/ana/Movies/kwikvid/2024-07-14/GX010042.MP4"),
        PathBuf::from("/home/ana/Movies/kwikvid/2024-07-14/GX020042.MP4"),
    ];
    let mut done = serde_json::to_value(CopyEvent::Done {
        copied,
        skipped: vec![],
    })
    .expect("done");
    let obj = done.as_object_mut().expect("object");
    obj.insert("id".into(), 1.into());
    obj.insert(
        "report".into(),
        serde_json::to_value(ImportReport {
            added: vec![MediaId(1)],
            skipped: vec![],
        })
        .expect("report"),
    );
    vec![
        ("camera-volumes", render(&vec![volume])),
        ("camera-recordings", render(&recordings)),
        ("camera-done", render(&done)),
    ]
}

/// The export dialog's fixtures: the timeline edit switched to 9:16 (what
/// `SetAspectRatio` returns), `export_options` answers and a finished
/// export's `kv://export` update.
fn export_fixtures() -> Vec<(&'static str, String)> {
    use kv_core::AspectRatio;
    use kv_media::VideoCodec;
    use kv_session::{ExportEvent, ExportOptions, ExportResolution, ExportSummary, ExportUpdate};
    let mut s = timeline_session();
    // The fixture's clips do not exist on this machine: the real blocker.
    let blocked = s.export_blocker(S);
    assert!(blocked.is_some(), "fixture media should be missing");
    let options = |codecs: Vec<VideoCodec>, blocked: Option<String>| ExportOptions {
        codecs,
        aspect: AspectRatio::Landscape16x9,
        resolution: ExportResolution::Hd1080,
        file_name: format!("{}.mp4", s.project_name()),
        blocked,
    };
    let all = options(vec![VideoCodec::H264, VideoCodec::Hevc], None);
    let h264 = options(vec![VideoCodec::H264], None);
    let missing = options(vec![VideoCodec::H264, VideoCodec::Hevc], blocked);
    let done = ExportUpdate {
        id: 1,
        event: ExportEvent::Done {
            report: ExportSummary {
                path: PathBuf::from("/home/ana/Movies/Untitled.mp4"),
                frames: 240,
                duration: Micros(8 * SEC),
                encoder_name: "h264_videotoolbox".into(),
                elapsed_ms: 12_345,
            },
        },
    };
    let portrait = s
        .apply(Command::SetAspectRatio {
            aspect: AspectRatio::Portrait9x16,
        })
        .expect("aspect");
    vec![
        ("export-options", render(&all)),
        ("export-options-h264", render(&h264)),
        ("export-options-missing", render(&missing)),
        ("export-done", render(&done)),
        ("export-portrait", render(&portrait)),
    ]
}

fn fixture_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("ui-tests/fixtures")
}

/// Pretty JSON in the type's own field order (not via `serde_json::Value`,
/// which would sort the keys).
fn render<T: serde::Serialize>(value: &T) -> String {
    let mut s = serde_json::to_string_pretty(value).expect("serialise fixture");
    s.push('\n');
    s
}

#[test]
fn ui_fixtures_match_real_snapshots() {
    let update = std::env::var_os("KV_UPDATE_UI_FIXTURES").is_some();
    let dir = fixture_dir();
    let mut stale = Vec::new();
    for (name, want) in all_fixtures() {
        let path = dir.join(format!("{name}.json"));
        if update {
            std::fs::write(&path, &want).expect("write fixture");
            continue;
        }
        // Compare as text after normalising line endings (Windows checkouts).
        let have = std::fs::read_to_string(&path)
            .unwrap_or_default()
            .replace("\r\n", "\n");
        if have != want {
            stale.push(path.display().to_string());
        }
    }
    assert!(
        stale.is_empty(),
        "UI fixtures are missing or out of date: {stale:?}\n\
         Regenerate: KV_UPDATE_UI_FIXTURES=1 cargo test -p kv-app --test ui_fixtures"
    );
}

/// No stray JSON files the generator no longer produces.
#[test]
fn every_fixture_file_is_generated() {
    let known: Vec<String> = all_fixtures()
        .iter()
        .map(|(n, _)| format!("{n}.json"))
        .collect();
    for entry in std::fs::read_dir(fixture_dir()).expect("read fixture dir") {
        let name = entry.expect("dir entry").file_name();
        let name = name.to_string_lossy();
        if name.ends_with(".json") {
            assert!(
                known.iter().any(|k| k == &name),
                "{name} is not generated by tests/ui_fixtures.rs; add it there or delete it"
            );
        }
    }
}

#[test]
fn fixtures_cover_every_undo_redo_combination() {
    let combos: Vec<(bool, bool)> = fixtures()
        .iter()
        .map(|(_, s)| (s.can_undo, s.can_redo))
        .collect();
    for want in [(false, false), (true, false), (false, true), (true, true)] {
        assert!(combos.contains(&want), "no fixture with {want:?}");
    }
}

/// The `get_beats` fixture has the same shape as the app's `Beats` (keep
/// `beats_json` in step with `src/timeline.rs`).
#[test]
fn beats_fixture_has_the_get_beats_keys() {
    let (_, beats) = other_fixtures()
        .into_iter()
        .find(|(n, _)| *n == "beats-song")
        .expect("beats fixture");
    let beats: serde_json::Value = serde_json::from_str(&beats).unwrap();
    let mut keys: Vec<_> = beats.as_object().unwrap().keys().cloned().collect();
    keys.sort();
    assert_eq!(
        keys,
        ["beats", "downbeats", "duration", "music", "tempo_bpm"]
    );
}
