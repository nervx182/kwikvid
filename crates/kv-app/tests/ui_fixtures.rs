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
//! To add a fixture, add an entry to [`fixtures`].

use std::path::{Path, PathBuf};

use kv_core::{Command, MediaId, MediaItem, MediaKind, Micros, Project};
use kv_session::{Session, Snapshot};

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

fn clip(id: u64, path: &str) -> Command {
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
        p.apply(clip(id, &format!("clips/{name}.MP4"))).expect("add clip");
    }
    let file = dir.path().join("trip.kwikvid");
    p.save_as(&file).expect("save project");
    let (mut o, missing) = Session::open(&file).expect("open");
    assert_eq!(missing.len(), 3);
    o.continue_revisions(edited.revision);
    let mut opened = o.snapshot();
    opened.media_root = PathBuf::from(PROJECT_DIR);

    vec![
        ("initial", initial),
        ("edited", edited),
        ("undone", undone),
        ("redone", redone),
        ("undo-redo", both),
        ("saved", saved),
        ("opened", opened),
    ]
}

fn fixture_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("ui-tests/fixtures")
}

fn render(snapshot: &Snapshot) -> String {
    let mut s = serde_json::to_string_pretty(snapshot).expect("serialise snapshot");
    s.push('\n');
    s
}

#[test]
fn ui_fixtures_match_real_snapshots() {
    let update = std::env::var_os("KV_UPDATE_UI_FIXTURES").is_some();
    let dir = fixture_dir();
    let mut stale = Vec::new();
    for (name, snapshot) in fixtures() {
        let path = dir.join(format!("{name}.json"));
        let want = render(&snapshot);
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
    let known: Vec<String> = fixtures()
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
