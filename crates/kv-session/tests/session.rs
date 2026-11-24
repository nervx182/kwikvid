//! `Session`: apply, undo, redo, revision, dirty tracking and snapshots.

use kv_core::*;
use kv_session::{Session, SessionError, Snapshot};
use std::path::PathBuf;

const S: SectionId = Project::DEFAULT_SECTION;

fn secs(s: i64) -> Micros {
    Micros(s * 1_000_000)
}

#[cfg(not(windows))]
fn root() -> PathBuf {
    PathBuf::from("/home/ana")
}
#[cfg(windows)]
fn root() -> PathBuf {
    PathBuf::from(r"C:\Users\ana")
}

fn add_clip(id: u64) -> Command {
    Command::AddMedia {
        item: MediaItem {
            id: MediaId(id),
            path: format!("clips/GX0{id}.MP4"),
            duration: secs(10),
            kind: MediaKind::Video,
            hilights: vec![secs(1)],
            clip_audio: false,
        },
    }
}

fn set_seed(seed: u64) -> Command {
    Command::SetSeed { section: S, seed }
}

#[test]
fn new_session_is_clean() {
    let s = Session::new(root());
    let snap = s.snapshot();
    assert_eq!(snap.revision, 0);
    assert_eq!(snap.project, Project::new(ProjectId(1)));
    assert!(!snap.can_undo && !snap.can_redo && !snap.dirty);
    assert_eq!(snap.media_root, root());
}

#[test]
fn apply_undo_redo_bump_revision_and_track_history() {
    let mut s = Session::new(root());

    let a = s.apply(add_clip(1)).unwrap();
    assert_eq!(a.revision, 1);
    assert_eq!(a.project.media.len(), 1);
    assert!(a.can_undo && !a.can_redo && a.dirty);

    let b = s.apply(set_seed(7)).unwrap();
    assert_eq!(b.revision, 2);
    assert_eq!(b.project.section(S).unwrap().seed, 7);

    let u = s.undo().unwrap();
    assert_eq!(u.revision, 3);
    assert_eq!(u.project, a.project);
    assert!(u.can_undo && u.can_redo);

    let r = s.redo().unwrap();
    assert_eq!(r.revision, 4);
    assert_eq!(r.project, b.project);
    assert!(r.can_undo && !r.can_redo);

    s.undo().unwrap();
    let u2 = s.undo().unwrap();
    assert_eq!(u2.revision, 6);
    assert_eq!(u2.project, Project::new(ProjectId(1)));
    assert!(!u2.can_undo && u2.can_redo);
    // Back at the creation state: not dirty.
    assert!(!u2.dirty);

    // A new command clears the redo stack.
    let c = s.apply(set_seed(9)).unwrap();
    assert!(!c.can_redo);
    assert_eq!(s.snapshot(), c);
}

#[test]
fn undo_redo_with_nothing_to_do_change_nothing() {
    let mut s = Session::new(root());
    assert_eq!(s.undo(), Err(SessionError::NothingToUndo));
    assert_eq!(s.redo(), Err(SessionError::NothingToRedo));
    assert_eq!(s.revision(), 0);
    assert_eq!(SessionError::NothingToUndo.to_string(), "Nothing to undo.");
}

#[test]
fn rejected_command_leaves_revision_and_project_unchanged() {
    let mut s = Session::new(root());
    s.apply(add_clip(1)).unwrap();
    let before = s.snapshot();

    let err = s.apply(add_clip(1)).unwrap_err();
    assert_eq!(
        err,
        SessionError::Core(CoreError::DuplicateMedia(MediaId(1)))
    );
    assert_eq!(err.to_string(), "That clip is already in the project.");

    let err = s
        .apply(Command::SetSeed {
            section: SectionId(99),
            seed: 1,
        })
        .unwrap_err();
    assert!(matches!(
        err,
        SessionError::Core(CoreError::UnknownSection(_))
    ));

    assert_eq!(s.snapshot(), before);
    // The rejected commands did not become undo steps.
    s.undo().unwrap();
    assert!(!s.snapshot().can_undo);
}

#[test]
fn dirty_follows_mark_saved() {
    let mut s = Session::new(root());
    s.apply(add_clip(1)).unwrap();
    assert!(s.is_dirty());

    s.mark_saved();
    let saved = s.snapshot();
    assert!(!saved.dirty);
    // Saving is not a change: revision and history stay.
    assert_eq!(saved.revision, 1);
    assert!(saved.can_undo);

    assert!(s.apply(set_seed(3)).unwrap().dirty);
    assert!(!s.undo().unwrap().dirty, "undo back to the saved state");
    assert!(s.undo().unwrap().dirty, "undo past the saved state");
    assert!(!s.redo().unwrap().dirty);
}

#[test]
fn snapshot_json_round_trips_with_snake_case_keys() {
    let mut s = Session::new(root());
    s.apply(add_clip(1)).unwrap();
    let snap = s.apply(set_seed(5)).unwrap();

    let json = serde_json::to_string(&snap).unwrap();
    let back: Snapshot = serde_json::from_str(&json).unwrap();
    assert_eq!(back, snap);

    let value: serde_json::Value = serde_json::from_str(&json).unwrap();
    let keys: Vec<&str> = value
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    for key in [
        "revision",
        "project",
        "can_undo",
        "can_redo",
        "dirty",
        "media_root",
    ] {
        assert!(keys.contains(&key), "missing {key} in {json}");
    }
    assert_eq!(value["project"]["version"], 1);
    assert_eq!(value["media_root"], root().to_str().unwrap());
}

#[test]
fn project_paths_from_session_feed_commands() {
    let mut s = Session::new(root());
    let file = root().join("Movies").join("GX01.MP4");
    let path = s.to_project_path(&file).unwrap();
    assert_eq!(path, "Movies/GX01.MP4");
    assert_eq!(s.resolve(&path), file);

    // kv-core accepts the relative path the session produced.
    let mut item = match add_clip(1) {
        Command::AddMedia { item } => item,
        _ => unreachable!(),
    };
    item.path = path;
    s.apply(Command::AddMedia { item }).unwrap();

    assert!(matches!(
        s.to_project_path(&root()),
        Err(SessionError::PathIsRoot(_))
    ));
    assert!(matches!(
        s.to_project_path(std::path::Path::new("relative.mp4")),
        Err(SessionError::PathNotAbsolute(_))
    ));
}
