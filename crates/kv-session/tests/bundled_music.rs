//! Bundled music is copied into the project folder on save (issue #130,
//! ADR 0013). Uses a temp "resource" folder and a temp project folder.

use kv_audio::MusicAnalysis;
use kv_core::*;
use kv_session::{Job, JobEvent, JobId, JobMessage, JobResult, Session};
use std::fs;
#[cfg(windows)]
use std::path::Path;
use std::path::PathBuf;

const S: SectionId = Project::DEFAULT_SECTION;

struct Fixture {
    _tmp: tempfile::TempDir,
    /// The app's bundled music folder.
    resources: PathBuf,
    /// The bundled track.
    track: PathBuf,
    /// An untitled project's media root (home).
    home: PathBuf,
}

fn fixture() -> Fixture {
    let tmp = tempfile::tempdir().unwrap();
    let resources = tmp.path().join("app").join("Resources").join("music");
    fs::create_dir_all(&resources).unwrap();
    let track = resources.join("arpent.mp3");
    fs::write(&track, b"bundled audio").unwrap();
    let home = tmp.path().join("home");
    fs::create_dir_all(&home).unwrap();
    Fixture {
        resources,
        track,
        home,
        _tmp: tmp,
    }
}

fn analysis() -> MusicAnalysis {
    MusicAnalysis {
        duration: Micros(30_000_000),
        tempo_bpm: 120.0,
        beats: Vec::new(),
        downbeats: Vec::new(),
        onsets: Vec::new(),
        energy: Vec::new(),
    }
}

/// An untitled session that picked the bundled track, as the app does:
/// analysis delivered, then `SetMusic` with the music project path.
fn picked(f: &Fixture) -> Session {
    let mut s = Session::new(f.home.clone());
    s.set_bundled_music_dir(Some(f.resources.clone()));
    s.check_music_path(&f.track).unwrap();
    s.handle_job_message(&JobMessage {
        id: JobId(1),
        job: Job::AnalyzeMusic(f.track.clone()),
        event: JobEvent::Done {
            result: JobResult::MusicAnalysis(analysis()),
        },
    });
    let path = s.music_project_path(&f.track).unwrap();
    s.apply(Command::SetMusic {
        section: S,
        music: Some(MusicRef {
            path,
            duration: Micros(30_000_000),
        }),
    })
    .unwrap();
    s
}

fn music_path(s: &Session) -> String {
    s.project().sections[0].music.as_ref().unwrap().path.clone()
}

#[test]
fn save_as_copies_bundled_music_into_the_project_folder() {
    let f = fixture();
    let mut s = picked(&f);
    // Referenced in place while untitled (ADR 0013).
    assert!(music_path(&s).starts_with("../app/Resources/music/"));

    fs::create_dir_all(f.home.join("trip")).unwrap();
    let file = f.home.join("trip").join("trip.kwikvid");
    let snap = s.save_as(&file).unwrap();
    let copy = f.home.join("trip").join("music").join("arpent.mp3");
    assert_eq!(fs::read(&copy).unwrap(), b"bundled audio");
    assert_eq!(music_path(&s), "music/arpent.mp3");
    assert_eq!(s.resolve(&music_path(&s)), copy);
    assert!(!snap.dirty);
    // The project file on disk has the rewritten path.
    let text = fs::read_to_string(&file).unwrap();
    assert!(text.contains("\"music/arpent.mp3\""), "{text}");
    assert!(!text.contains("Resources"), "{text}");
    // The analysis follows the copy, and no temp file is left behind.
    assert!(s
        .music_analysis(s.project().sections[0].music.as_ref().unwrap())
        .is_some());
    let mut names: Vec<_> = fs::read_dir(copy.parent().unwrap())
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    names.sort();
    assert_eq!(names, ["arpent.mp3"]);

    // Undo and redo states point at the copy too.
    let undone = s.undo().unwrap();
    assert!(undone.project.sections[0].music.is_none());
    let redone = s.redo().unwrap();
    assert_eq!(
        redone.project.sections[0].music.as_ref().unwrap().path,
        "music/arpent.mp3"
    );
}

#[test]
fn second_save_does_not_copy_again() {
    let f = fixture();
    let mut s = picked(&f);
    let file = f.home.join("trip.kwikvid");
    s.save_as(&file).unwrap();
    let copy = f.home.join("music").join("arpent.mp3");
    // Mark the copy: a new copy would overwrite it.
    fs::write(&copy, b"marked").unwrap();
    let revision = s.revision();
    s.save().unwrap();
    assert_eq!(fs::read(&copy).unwrap(), b"marked");
    assert_eq!(s.revision(), revision);
    assert_eq!(music_path(&s), "music/arpent.mp3");
}

#[test]
fn saved_project_opens_without_missing_music_after_the_app_moves() {
    let f = fixture();
    let mut s = picked(&f);
    let file = f.home.join("trip.kwikvid");
    s.save_as(&file).unwrap();
    // Move the app.
    let app = f.resources.parent().unwrap().parent().unwrap();
    fs::rename(app, app.with_file_name("moved-app")).unwrap();
    let (opened, missing) = Session::open(&file).unwrap();
    assert!(missing.is_empty(), "{missing:?}");
    assert_eq!(music_path(&opened), "music/arpent.mp3");
}

#[test]
fn save_copies_in_place_references_of_an_opened_project() {
    // A project saved before #130 refers to the track where the app is.
    let f = fixture();
    let mut s = Session::new(f.home.clone());
    let path = s.to_project_path(&f.track).unwrap();
    s.apply(Command::SetMusic {
        section: S,
        music: Some(MusicRef {
            path,
            duration: Micros(30_000_000),
        }),
    })
    .unwrap();
    let file = f.home.join("old.kwikvid");
    s.save_as(&file).unwrap();
    assert!(music_path(&s).contains("Resources"));

    let (mut opened, _) = Session::open(&file).unwrap();
    opened.set_bundled_music_dir(Some(f.resources.clone()));
    let revision = opened.revision();
    opened.save().unwrap();
    assert_eq!(music_path(&opened), "music/arpent.mp3");
    assert!(f.home.join("music").join("arpent.mp3").is_file());
    // The path changed, so the UI must see a new revision.
    assert_eq!(opened.revision(), revision + 1);
    assert!(!opened.is_dirty());
}

#[test]
fn existing_files_are_reused_or_kept() {
    let f = fixture();
    let music = f.home.join("trip").join("music");
    fs::create_dir_all(&music).unwrap();

    // Same contents: reused, not copied.
    fs::write(music.join("arpent.mp3"), b"bundled audio").unwrap();
    let mut s = picked(&f);
    s.save_as(&f.home.join("trip").join("a.kwikvid")).unwrap();
    assert_eq!(music_path(&s), "music/arpent.mp3");

    // Other contents (the user's own file): kept, the copy is numbered.
    fs::write(music.join("arpent.mp3"), b"user's song").unwrap();
    let mut s = picked(&f);
    s.save_as(&f.home.join("trip").join("b.kwikvid")).unwrap();
    assert_eq!(music_path(&s), "music/arpent-2.mp3");
    assert_eq!(fs::read(music.join("arpent.mp3")).unwrap(), b"user's song");
    assert_eq!(
        fs::read(music.join("arpent-2.mp3")).unwrap(),
        b"bundled audio"
    );
}

#[test]
fn failed_save_leaves_no_copy_behind() {
    let f = fixture();
    let mut s = picked(&f);
    // The project file's location is a folder, so writing it fails.
    let file = f.home.join("trip.kwikvid");
    fs::create_dir_all(&file).unwrap();
    let before = s.snapshot();
    assert!(s.save_as(&file).is_err());
    assert!(!f.home.join("music").join("arpent.mp3").exists());
    assert_eq!(s.snapshot(), before);
    assert_eq!(s.file_path(), None);
}

#[test]
fn without_a_bundled_folder_nothing_is_copied() {
    let f = fixture();
    let mut s = picked(&f);
    s.set_bundled_music_dir(None);
    s.save_as(&f.home.join("trip.kwikvid")).unwrap();
    assert!(music_path(&s).contains("Resources"));
    assert!(!f.home.join("music").exists());
}

#[test]
fn non_bundled_music_is_never_copied() {
    let f = fixture();
    let mut s = Session::new(f.home.clone());
    s.set_bundled_music_dir(Some(f.resources.clone()));
    let song = f.home.join("Music").join("song.mp3");
    fs::create_dir_all(song.parent().unwrap()).unwrap();
    fs::write(&song, b"x").unwrap();
    let path = s.music_project_path(&song).unwrap();
    s.apply(Command::SetMusic {
        section: S,
        music: Some(MusicRef {
            path,
            duration: Micros(1),
        }),
    })
    .unwrap();
    fs::create_dir_all(f.home.join("p")).unwrap();
    s.save_as(&f.home.join("p").join("p.kwikvid")).unwrap();
    assert_eq!(music_path(&s), "../Music/song.mp3");
    assert!(!f.home.join("p").join("music").exists());
}

/// On Windows a bundled track on another drive than the project can be
/// picked; it is copied on save. Needs the app's drive to differ from the
/// temp folder's, so it builds the paths by hand and only checks the pick.
#[cfg(windows)]
#[test]
fn bundled_track_on_another_drive_can_be_picked() {
    let mut s = Session::new(PathBuf::from(r"D:\trips\p"));
    s.set_bundled_music_dir(Some(PathBuf::from(r"C:\kwikvid\music")));
    let track = Path::new(r"C:\kwikvid\music\arpent.mp3");
    s.check_music_path(track).unwrap();
    assert_eq!(s.music_project_path(track).unwrap(), "music/arpent.mp3");
    assert_eq!(s.resolve("music/arpent.mp3"), track);
}
