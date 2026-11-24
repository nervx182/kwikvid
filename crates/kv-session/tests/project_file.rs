//! Project files: save, save as, open, missing media, atomic writes and the
//! recent projects list. Uses temp folders with empty placeholder media.

use kv_core::*;
use kv_session::{
    MissingFile, MissingKind, ProjectFileError, RecentProjects, Session, SessionError,
};
use std::fs;
use std::path::{Path, PathBuf};

const S: SectionId = Project::DEFAULT_SECTION;

fn secs(s: i64) -> Micros {
    Micros(s * 1_000_000)
}

/// Create an empty placeholder file (and its folders).
fn touch(path: &Path) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, b"").unwrap();
}

/// Copy a folder tree.
fn copy_dir(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for entry in fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let dest = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_dir(&entry.path(), &dest);
        } else {
            fs::copy(entry.path(), dest).unwrap();
        }
    }
}

fn file_names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    names
}

/// A session rooted at `root` with two clips under `root/trip/clips` and
/// music under `root/music`, all existing as placeholders, plus an edit.
fn session_with_media(root: &Path) -> Session {
    let mut s = Session::new(root.to_owned());
    for id in 1..=2 {
        let file = root.join("trip").join("clips").join(format!("GX0{id}.MP4"));
        touch(&file);
        let path = s.to_project_path(&file).unwrap();
        s.apply(Command::AddMedia {
            item: MediaItem {
                id: MediaId(id),
                path,
                duration: secs(10),
                kind: MediaKind::Video,
                hilights: vec![secs(2)],
                clip_audio: false,
            },
        })
        .unwrap();
    }
    let song = root.join("music").join("song.mp3");
    touch(&song);
    let path = s.to_project_path(&song).unwrap();
    s.apply(Command::SetMusic {
        section: S,
        music: Some(MusicRef {
            path,
            duration: secs(30),
        }),
    })
    .unwrap();
    s.apply(Command::SetSeed {
        section: S,
        seed: 42,
    })
    .unwrap();
    s
}

fn paths_of(p: &Project) -> Vec<String> {
    let mut v: Vec<String> = p.media.iter().map(|m| m.path.clone()).collect();
    v.extend(
        p.sections
            .iter()
            .filter_map(|s| s.music.as_ref().map(|m| m.path.clone())),
    );
    v
}

#[test]
fn save_as_then_open_round_trips() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let mut s = session_with_media(root);
    let before = s.revision();

    let file = root.join("trip").join("trip.kwikvid");
    let snap = s.save_as(&file).unwrap();
    assert!(!snap.dirty);
    assert_eq!(snap.revision, before + 1);
    assert_eq!(snap.media_root, root.join("trip"));
    assert_eq!(s.file_path(), Some(file.as_path()));
    assert_eq!(
        paths_of(&snap.project),
        ["clips/GX01.MP4", "clips/GX02.MP4", "../music/song.mp3"]
    );
    // Undo history survives the rebase and stays consistent with the new root.
    assert!(snap.can_undo);

    // Pretty-printed JSON with the version first.
    let text = fs::read_to_string(&file).unwrap();
    assert!(text.starts_with("{\n  \"version\": 1,"), "{text}");

    let (opened, missing) = Session::open(&file).unwrap();
    assert!(missing.is_empty(), "{missing:?}");
    assert_eq!(opened.project(), s.project());
    assert_eq!(opened.media_root(), root.join("trip"));
    assert_eq!(opened.file_path(), Some(file.as_path()));
    assert!(!opened.is_dirty());
    assert!(!opened.snapshot().can_undo);
}

#[test]
fn save_as_appends_extension_and_undo_uses_rebased_paths() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let mut s = session_with_media(root);
    let snap = s.save_as(&root.join("trip").join("trip")).unwrap();
    assert_eq!(
        s.file_path(),
        Some(root.join("trip/trip.kwikvid").as_path())
    );
    assert!(root.join("trip/trip.kwikvid").is_file());

    // Undo the seed and the music: the remaining paths are still relative
    // to the new root, so they resolve to the real files.
    s.undo().unwrap();
    let after_undo = s.undo().unwrap();
    assert!(after_undo.dirty);
    for m in &after_undo.project.media {
        assert!(s.resolve(&m.path).is_file(), "{}", m.path);
    }
    s.redo().unwrap();
    let redone = s.redo().unwrap();
    assert_eq!(redone.project, snap.project);
    assert!(!redone.dirty);
}

#[test]
fn save_as_into_sibling_folder_uses_dot_dot() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let mut s = session_with_media(root);
    s.save_as(&root.join("trip").join("trip.kwikvid")).unwrap();

    let sibling = root.join("projects");
    fs::create_dir_all(&sibling).unwrap();
    let file = sibling.join("copy.kwikvid");
    let snap = s.save_as(&file).unwrap();
    assert_eq!(
        paths_of(&snap.project),
        [
            "../trip/clips/GX01.MP4",
            "../trip/clips/GX02.MP4",
            "../music/song.mp3"
        ]
    );
    let (opened, missing) = Session::open(&file).unwrap();
    assert!(missing.is_empty(), "{missing:?}");
    assert_eq!(opened.project(), s.project());
}

#[test]
fn moved_project_folder_still_resolves_media() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("orig");
    let mut s = session_with_media(&root);
    s.save_as(&root.join("project.kwikvid")).unwrap();

    let moved = tempfile::tempdir().unwrap();
    let dest = moved.path().join("elsewhere");
    copy_dir(&root, &dest);
    fs::remove_dir_all(&root).unwrap();

    let (opened, missing) = Session::open(&dest.join("project.kwikvid")).unwrap();
    assert!(missing.is_empty(), "{missing:?}");
    assert_eq!(opened.media_root(), dest);
    for m in &opened.project().media {
        let resolved = opened.resolve(&m.path);
        assert!(resolved.starts_with(&dest) && resolved.is_file());
    }
}

#[test]
fn open_reports_missing_media_and_music() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let mut s = session_with_media(root);
    let file = root.join("trip").join("trip.kwikvid");
    s.save_as(&file).unwrap();
    fs::remove_file(root.join("trip/clips/GX02.MP4")).unwrap();
    fs::remove_file(root.join("music/song.mp3")).unwrap();

    let (opened, missing) = Session::open(&file).unwrap();
    assert_eq!(
        missing,
        vec![
            MissingFile {
                kind: MissingKind::Media {
                    media_id: MediaId(2)
                },
                project_path: "clips/GX02.MP4".into(),
                resolved: opened.resolve("clips/GX02.MP4"),
            },
            MissingFile {
                kind: MissingKind::Music { section: S },
                project_path: "../music/song.mp3".into(),
                resolved: opened.resolve("../music/song.mp3"),
            },
        ]
    );
    assert_eq!(missing[1].resolved, root.join("music").join("song.mp3"));
    // The project still opens in full.
    assert_eq!(opened.project().media.len(), 2);
}

#[test]
fn save_writes_to_current_file_and_requires_one() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let mut s = session_with_media(root);
    assert!(matches!(s.save(), Err(ProjectFileError::Untitled)));
    assert!(s.is_dirty(), "a failed save changes nothing");

    let file = root.join("p.kwikvid");
    s.save_as(&file).unwrap();
    s.apply(Command::SetSeed {
        section: S,
        seed: 7,
    })
    .unwrap();
    let rev = s.revision();
    let snap = s.save().unwrap();
    assert!(!snap.dirty);
    assert_eq!(snap.revision, rev);
    let (opened, _) = Session::open(&file).unwrap();
    assert_eq!(opened.project().section(S).unwrap().seed, 7);
}

#[test]
fn atomic_write_leaves_no_temp_file() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("p");
    let mut s = session_with_media(&root);
    let file = root.join("p.kwikvid");
    s.save_as(&file).unwrap();
    s.save().unwrap(); // overwrite an existing file
    assert_eq!(file_names(&root), ["music", "p.kwikvid", "trip"]);

    // A failed write (the target is a folder) leaves no temp file and no
    // change to the session.
    let blocked = root.join("blocked.kwikvid");
    fs::create_dir(&blocked).unwrap();
    let before = s.snapshot();
    let err = s.save_as(&blocked).unwrap_err();
    assert!(matches!(err, ProjectFileError::Io { .. }), "{err:?}");
    assert_eq!(s.snapshot(), before);
    assert_eq!(s.file_path(), Some(file.as_path()));
    assert_eq!(
        file_names(&root),
        ["blocked.kwikvid", "music", "p.kwikvid", "trip"]
    );
}

#[test]
fn corrupt_and_unsupported_files_give_readable_errors() {
    let tmp = tempfile::tempdir().unwrap();
    let bad = tmp.path().join("bad.kwikvid");

    fs::write(&bad, "{ not json").unwrap();
    let err = Session::open(&bad).unwrap_err();
    assert!(matches!(err, ProjectFileError::Corrupt { .. }), "{err:?}");
    assert_eq!(
        err.to_string(),
        "That file is damaged or is not a kwikvid project."
    );

    fs::write(&bad, r#"{"version":1,"id":1}"#).unwrap();
    assert!(matches!(
        Session::open(&bad),
        Err(ProjectFileError::Corrupt { .. })
    ));

    fs::write(&bad, r#"{"version":7,"id":1,"media":[]}"#).unwrap();
    let err = Session::open(&bad).unwrap_err();
    assert!(
        matches!(&err, ProjectFileError::UnsupportedVersion { found, .. } if found == "7"),
        "{err:?}"
    );
    assert!(err.to_string().contains("different version of kwikvid"));

    let err = Session::open(&tmp.path().join("nope.kwikvid")).unwrap_err();
    assert!(matches!(err, ProjectFileError::Io { .. }));
}

#[test]
fn recent_projects_keep_last_ten_newest_first() {
    let tmp = tempfile::tempdir().unwrap();
    let config = tmp.path().join("config").join("kwikvid");

    // No file yet: empty.
    assert_eq!(
        RecentProjects::load(&config).unwrap().paths(),
        &[] as &[PathBuf]
    );

    let mut recent = RecentProjects::default();
    for i in 0..12 {
        recent.add(&PathBuf::from(format!("/p/{i}.kwikvid")));
    }
    // Re-adding moves to the top without duplicating.
    recent.add(Path::new("/p/5.kwikvid"));
    assert_eq!(recent.paths().len(), RecentProjects::LIMIT);
    assert_eq!(recent.paths()[0], PathBuf::from("/p/5.kwikvid"));
    assert_eq!(recent.paths()[1], PathBuf::from("/p/11.kwikvid"));
    assert!(!recent.paths().contains(&PathBuf::from("/p/1.kwikvid")));

    recent.save(&config).unwrap();
    assert_eq!(file_names(&config), [RecentProjects::FILE_NAME]);
    assert_eq!(RecentProjects::load(&config).unwrap(), recent);

    recent.remove(Path::new("/p/5.kwikvid"));
    assert_eq!(recent.paths()[0], PathBuf::from("/p/11.kwikvid"));

    fs::write(config.join(RecentProjects::FILE_NAME), "[").unwrap();
    assert!(matches!(
        RecentProjects::load(&config),
        Err(ProjectFileError::Corrupt { .. })
    ));
}

#[cfg(windows)]
#[test]
fn save_as_onto_another_drive_fails_without_changes() {
    let mut s = Session::new(PathBuf::from(r"C:\Users\ana"));
    s.apply(Command::AddMedia {
        item: MediaItem {
            id: MediaId(1),
            path: "a.mp4".into(),
            duration: secs(1),
            kind: MediaKind::Video,
            hilights: vec![],
            clip_audio: false,
        },
    })
    .unwrap();
    let before = s.snapshot();
    let err = s.save_as(Path::new(r"Z:\nowhere\p.kwikvid")).unwrap_err();
    assert!(matches!(
        err,
        ProjectFileError::Path(SessionError::OtherVolume(_))
    ));
    assert_eq!(s.snapshot(), before);
}

#[cfg(not(windows))]
#[test]
fn rebase_errors_surface_as_path_errors() {
    // A relative media root cannot be rebased.
    let tmp = tempfile::tempdir().unwrap();
    let mut s = Session::new(PathBuf::from("relative-root"));
    s.apply(Command::AddMedia {
        item: MediaItem {
            id: MediaId(1),
            path: "a.mp4".into(),
            duration: secs(1),
            kind: MediaKind::Video,
            hilights: vec![],
            clip_audio: false,
        },
    })
    .unwrap();
    let err = s.save_as(&tmp.path().join("p.kwikvid")).unwrap_err();
    assert!(
        matches!(
            err,
            ProjectFileError::Path(SessionError::PathNotAbsolute(_))
        ),
        "{err:?}"
    );
    assert!(file_names(tmp.path()).is_empty());
}
