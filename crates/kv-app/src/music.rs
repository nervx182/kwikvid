//! Music picker IPC (`ui/js/music.js`): the bundled track list, the file
//! picker, and the analyse-then-set flow (ADR 0012).
//!
//! - `bundled_tracks()` lists the tracks in the app resource
//!   `music/manifest.toml` (copied from `assets/music/`) whose file is
//!   present; an empty list, not an error, when the manifest or the files
//!   are missing.
//! - `pick_music()` opens the native picker filtered to
//!   [`MUSIC_EXTENSIONS`] and returns the absolute path, or `null`.
//! - `analyze_music(section, path)` checks the file can be referenced from
//!   the project, submits [`Job::AnalyzeMusic`] and returns its queued
//!   [`JobUpdate`]. When that job is `"done"` the job event thread applies
//!   `Command::SetMusic` (project path, analysed duration) through
//!   [`Session::apply_and_regenerate`] and emits `kv://state` before the
//!   job's `kv://job` update; when it fails nothing changes and the update
//!   carries the message. Only the latest pick per section is applied.
//!
//! Removing music is a plain `Command::SetMusic { music: None }` through
//! `apply`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use kv_core::{Command, MusicRef, SectionId};
use kv_session::regen::RegenSnapshot;
use kv_session::{Job, JobEvent, JobId, JobMessage, JobResult, Session};
use serde::{Deserialize, Serialize};
use tauri::path::BaseDirectory;
use tauri::{AppHandle, Manager, Runtime, State};
use tauri_plugin_dialog::DialogExt;

use crate::ipc::{AppState, IpcError};
use crate::library::{JobUpdate, Library};

/// The audio files the picker offers (lower case; the picker also accepts
/// upper case). All are decoded by `kv-audio`.
pub const MUSIC_EXTENSIONS: [&str; 5] = ["mp3", "m4a", "aac", "wav", "flac"];

/// The app resource folder holding the bundled tracks and their manifest
/// (`bundle.resources` in `tauri.conf.json`).
const RESOURCE_DIR: &str = "music";

/// One bundled track, as the UI lists it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BundledTrack {
    pub id: String,
    pub title: String,
    pub artist: String,
    /// Absolute path of the file in the app resources: what the UI passes
    /// to `analyze_music`.
    pub path: PathBuf,
}

/// `assets/music/manifest.toml` (schema in that file, issue #48).
#[derive(Debug, Deserialize)]
struct Manifest {
    #[serde(default)]
    track: Vec<ManifestTrack>,
}

#[derive(Debug, Deserialize)]
struct ManifestTrack {
    id: String,
    title: String,
    artist: String,
    file: String,
    licence: String,
}

/// The tracks of manifest `text` whose file exists in `dir`. Entries that
/// are not CC0-1.0 or whose `file` is not a plain file name are skipped.
pub fn parse_manifest(text: &str, dir: &Path) -> Result<Vec<BundledTrack>, toml::de::Error> {
    let manifest: Manifest = toml::from_str(text)?;
    Ok(manifest
        .track
        .into_iter()
        .filter(|t| {
            let plain = !t.file.is_empty()
                && Path::new(&t.file).file_name() == Some(t.file.as_ref())
                && t.file != "..";
            if !plain || t.licence != "CC0-1.0" {
                tracing::warn!(id = %t.id, file = %t.file, licence = %t.licence, "bundled track skipped");
                return false;
            }
            true
        })
        .map(|t| BundledTrack {
            path: dir.join(&t.file),
            id: t.id,
            title: t.title,
            artist: t.artist,
        })
        .filter(|t| t.path.is_file())
        .collect())
}

/// The bundled tracks in `dir` (the resource folder): empty when there is
/// no folder, no manifest, an unreadable one, or no track file present.
pub fn bundled_in(dir: Option<&Path>) -> Vec<BundledTrack> {
    let Some(dir) = dir else {
        return Vec::new();
    };
    let manifest = dir.join("manifest.toml");
    let text = match std::fs::read_to_string(&manifest) {
        Ok(text) => text,
        Err(e) => {
            tracing::debug!(?manifest, error = %e, "no bundled music manifest");
            return Vec::new();
        }
    };
    parse_manifest(&text, dir).unwrap_or_else(|e| {
        tracing::warn!(?manifest, error = %e, "bundled music manifest unreadable");
        Vec::new()
    })
}

/// Whether `path` has one of the [`MUSIC_EXTENSIONS`] (any case).
pub fn is_music_file(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| MUSIC_EXTENSIONS.contains(&e.to_lowercase().as_str()))
}

/// Tauri managed state: the analysis job each section is waiting for.
#[derive(Debug, Default)]
pub struct MusicPicks {
    pending: Mutex<HashMap<SectionId, JobId>>,
}

impl MusicPicks {
    fn pending(&self) -> MutexGuard<'_, HashMap<SectionId, JobId>> {
        self.pending.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// Validate a pick and submit its analysis with `submit`; remembers the job
/// as `section`'s pending pick (replacing an earlier one, whose result will
/// no longer be applied) and returns `submit`'s queued update. Nothing is
/// submitted when the section does not exist, the file is not a supported
/// format, or it cannot be referenced from the project.
pub fn start(
    session: &Session,
    picks: &MusicPicks,
    section: SectionId,
    path: &Path,
    submit: impl FnOnce(Job) -> Result<JobUpdate, IpcError>,
) -> Result<JobUpdate, IpcError> {
    if session.project().section(section).is_none() {
        return Err(IpcError {
            message: "That part of the project no longer exists.".into(),
        });
    }
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string());
    if !is_music_file(path) {
        return Err(IpcError {
            message: format!("\"{name}\" is not a supported song. Use MP3, AAC, WAV or FLAC."),
        });
    }
    session.to_project_path(path).map_err(|e| IpcError {
        message: format!("\"{name}\" can't be used as music here: {e}"),
    })?;
    let update = submit(Job::AnalyzeMusic(path.to_path_buf()))?;
    picks.pending().insert(section, JobId(update.id));
    Ok(update)
}

/// Handle a job message for the music picker, after
/// [`Session::handle_job_message`] stored any analysis. For the final event
/// of a section's pending pick: on `Done`, apply `SetMusic` and return the
/// new state; on `Failed` or `Cancelled`, forget the pick. `None` for every
/// other message. An `Err` means the analysis worked but the music could
/// not be set (the section is gone, say): nothing changed.
pub fn finish(
    session: &mut Session,
    picks: &MusicPicks,
    msg: &JobMessage,
) -> Option<Result<RegenSnapshot, IpcError>> {
    let Job::AnalyzeMusic(path) = &msg.job else {
        return None;
    };
    if !msg.event.is_final() {
        return None;
    }
    let section = {
        let mut pending = picks.pending();
        let section = pending
            .iter()
            .find_map(|(s, id)| (*id == msg.id).then_some(*s))?;
        pending.remove(&section);
        section
    };
    let JobEvent::Done {
        result: JobResult::MusicAnalysis(analysis),
    } = &msg.event
    else {
        return None;
    };
    let set = || -> Result<RegenSnapshot, IpcError> {
        let music = MusicRef {
            path: session.to_project_path(path)?,
            duration: analysis.duration,
        };
        let analyses = session.analyses();
        Ok(session.apply_and_regenerate(
            Command::SetMusic {
                section,
                music: Some(music),
            },
            &analyses,
        )?)
    };
    Some(set())
}

/// The job event thread's hook (see `library.rs`): runs [`finish`], emits
/// the new state, and turns `update` into a failure when the music could not
/// be set.
pub fn on_job_message<R: Runtime>(
    app: &AppHandle<R>,
    session: &mut Session,
    msg: &JobMessage,
    update: &mut JobUpdate,
) {
    let Some(picks) = app.try_state::<MusicPicks>() else {
        return;
    };
    match finish(session, &picks, msg) {
        Some(Ok(snapshot)) => crate::emit_state(app, &snapshot),
        Some(Err(e)) => {
            update.state = "failed";
            update.message = Some(e.message);
        }
        None => {}
    }
}

/// The bundled tracks shipped with the app (possibly none).
#[tauri::command]
pub fn bundled_tracks(app: AppHandle) -> Vec<BundledTrack> {
    let dir = app
        .path()
        .resolve(RESOURCE_DIR, BaseDirectory::Resource)
        .ok();
    bundled_in(dir.as_deref())
}

/// Let the user pick one song; `None` when the picker is cancelled.
#[tauri::command]
pub async fn pick_music(app: AppHandle) -> Result<Option<PathBuf>, IpcError> {
    tauri::async_runtime::spawn_blocking(move || {
        let exts: Vec<String> = MUSIC_EXTENSIONS
            .iter()
            .flat_map(|e| [e.to_string(), e.to_uppercase()])
            .collect();
        let exts: Vec<&str> = exts.iter().map(String::as_str).collect();
        app.dialog()
            .file()
            .set_title("Choose music")
            .add_filter("Music", &exts)
            .blocking_pick_file()
            .and_then(|p| p.into_path().ok())
    })
    .await
    .map_err(|e| IpcError {
        message: format!("The file picker failed: {e}"),
    })
}

/// Analyse `path` (absolute) as `section`'s music; it is set when the
/// analysis is done (see the module docs). Resolves to the queued job.
#[tauri::command]
pub fn analyze_music(
    app: AppHandle,
    state: State<'_, AppState>,
    picks: State<'_, MusicPicks>,
    library: State<'_, Library>,
    section: SectionId,
    path: PathBuf,
) -> Result<JobUpdate, IpcError> {
    let update = start(&*state.session()?, &picks, section, &path, |job| {
        library.submit(job)
    })?;
    crate::library::emit_job(&app, &update);
    Ok(update)
}

#[cfg(test)]
mod tests {
    use super::*;
    use kv_audio::MusicAnalysis;
    use kv_core::{Micros, Project};

    const S: SectionId = Project::DEFAULT_SECTION;
    const SEC: i64 = 1_000_000;

    const MANIFEST: &str = r#"
version = 1

[[track]]
id = "arpent"
title = "Arpent"
artist = "Kevin MacLeod"
file = "arpent.mp3"
licence = "CC0-1.0"
sha256 = ""
source_url = "https://freepd.com/electronic.php"

[[track]]
id = "absent"
title = "Absent"
artist = "Nobody"
file = "absent.mp3"
licence = "CC0-1.0"
sha256 = ""
source_url = "https://freepd.com/electronic.php"

[[track]]
id = "cc-by"
title = "Attribution"
artist = "Somebody"
file = "cc-by.mp3"
licence = "CC-BY-4.0"
sha256 = ""
source_url = "https://example.com"

[[track]]
id = "escape"
title = "Escape"
artist = "Somebody"
file = "../escape.mp3"
licence = "CC0-1.0"
sha256 = ""
source_url = "https://example.com"
"#;

    #[test]
    fn manifest_lists_present_cc0_tracks_only() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("music");
        std::fs::create_dir(&dir).unwrap();
        for f in ["arpent.mp3", "cc-by.mp3"] {
            std::fs::write(dir.join(f), b"mp3").unwrap();
        }
        std::fs::write(root.path().join("escape.mp3"), b"mp3").unwrap();
        let tracks = parse_manifest(MANIFEST, &dir).unwrap();
        assert_eq!(
            tracks,
            vec![BundledTrack {
                id: "arpent".into(),
                title: "Arpent".into(),
                artist: "Kevin MacLeod".into(),
                path: dir.join("arpent.mp3"),
            }]
        );
        assert_eq!(
            serde_json::to_value(&tracks[0]).unwrap(),
            serde_json::json!({"id": "arpent", "title": "Arpent", "artist": "Kevin MacLeod",
                "path": dir.join("arpent.mp3")})
        );
    }

    #[test]
    fn missing_or_broken_manifest_is_an_empty_list() {
        assert!(bundled_in(None).is_empty());
        let dir = tempfile::tempdir().unwrap();
        assert!(bundled_in(Some(dir.path())).is_empty());
        assert!(bundled_in(Some(&dir.path().join("nope"))).is_empty());
        std::fs::write(dir.path().join("manifest.toml"), "not [ toml").unwrap();
        assert!(bundled_in(Some(dir.path())).is_empty());
        std::fs::write(dir.path().join("manifest.toml"), "version = 1\n").unwrap();
        assert!(bundled_in(Some(dir.path())).is_empty());
    }

    /// The checked-in manifest parses; its audio is not committed yet (#67),
    /// so it lists nothing, without an error.
    #[test]
    fn repository_manifest_parses() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../assets/music");
        let text = std::fs::read_to_string(dir.join("manifest.toml")).unwrap();
        let tracks = parse_manifest(&text, &dir).unwrap();
        for t in &tracks {
            assert!(t.path.is_file(), "{t:?}");
        }
        assert_eq!(bundled_in(Some(&dir)), tracks);
    }

    /// `bundle.resources` copies `assets/music/` to the `music` resource
    /// folder that `bundled_tracks` reads.
    #[test]
    fn config_bundles_the_music_folder() {
        let conf: serde_json::Value =
            serde_json::from_str(include_str!("../tauri.conf.json")).unwrap();
        assert_eq!(
            conf["bundle"]["resources"]["../../assets/music/"],
            format!("{RESOURCE_DIR}/")
        );
    }

    #[test]
    fn music_extensions_match_the_picker_filter() {
        for ok in ["a.mp3", "b.M4A", "c.aac", "d.WAV", "e.flac"] {
            assert!(is_music_file(Path::new(ok)), "{ok}");
        }
        for bad in ["a.ogg", "b.mp4", "c", "d.mp3.txt"] {
            assert!(!is_music_file(Path::new(bad)), "{bad}");
        }
    }

    fn session() -> Session {
        Session::new(PathBuf::from("/home/ana"))
    }

    fn queued(id: u64) -> impl FnOnce(Job) -> Result<JobUpdate, IpcError> {
        move |job| Ok(JobUpdate::queued(JobId(id), &job))
    }

    fn analysis() -> MusicAnalysis {
        let beats: Vec<Micros> = (0..16).map(|i| Micros(i * SEC / 2)).collect();
        MusicAnalysis {
            duration: Micros(8 * SEC),
            tempo_bpm: 120.0,
            downbeats: beats.iter().step_by(4).copied().collect(),
            beats,
            onsets: Vec::new(),
            energy: Vec::new(),
        }
    }

    fn msg(id: u64, path: &str, event: JobEvent) -> JobMessage {
        JobMessage {
            id: JobId(id),
            job: Job::AnalyzeMusic(PathBuf::from(path)),
            event,
        }
    }

    fn done(id: u64, path: &str) -> JobMessage {
        msg(
            id,
            path,
            JobEvent::Done {
                result: JobResult::MusicAnalysis(analysis()),
            },
        )
    }

    /// What the job event thread does: store, then finish.
    fn deliver(
        s: &mut Session,
        picks: &MusicPicks,
        m: &JobMessage,
    ) -> Option<Result<RegenSnapshot, IpcError>> {
        s.handle_job_message(m);
        finish(s, picks, m)
    }

    #[test]
    fn done_sets_the_music_with_a_relative_path_and_the_analysed_duration() {
        let mut s = session();
        let picks = MusicPicks::default();
        let path = "/home/ana/Music/song.mp3";
        let u = start(&s, &picks, S, Path::new(path), queued(4)).unwrap();
        assert_eq!((u.id, u.kind, u.state), (4, "music-analysis", "queued"));
        // Progress changes nothing.
        let p = msg(4, path, JobEvent::Progress { fraction: 0.5 });
        assert!(deliver(&mut s, &picks, &p).is_none());
        assert_eq!(s.revision(), 0);

        let r = deliver(&mut s, &picks, &done(4, path)).unwrap().unwrap();
        let music = r.snapshot.project.sections[0].music.clone().unwrap();
        assert_eq!(music.path, "Music/song.mp3");
        assert_eq!(music.duration, Micros(8 * SEC));
        assert!(r.snapshot.can_undo);
        assert!(s.music_analysis(&music).is_some());
        // The pick is used once.
        assert!(deliver(&mut s, &picks, &done(4, path)).is_none());
        // Undo removes it again.
        assert!(s.undo().unwrap().project.sections[0].music.is_none());
    }

    #[test]
    fn bundled_track_outside_the_media_root_gets_a_dotdot_path() {
        let mut s = session();
        let picks = MusicPicks::default();
        let path = "/Applications/kwikvid.app/Contents/Resources/music/arpent.mp3";
        start(&s, &picks, S, Path::new(path), queued(1)).unwrap();
        let r = deliver(&mut s, &picks, &done(1, path)).unwrap().unwrap();
        let music = r.snapshot.project.sections[0].music.clone().unwrap();
        assert_eq!(
            music.path,
            "../../Applications/kwikvid.app/Contents/Resources/music/arpent.mp3"
        );
        assert_eq!(s.resolve(&music.path), PathBuf::from(path));
    }

    #[test]
    fn failure_leaves_the_music_unchanged() {
        let mut s = session();
        let picks = MusicPicks::default();
        start(&s, &picks, S, Path::new("/home/ana/a.mp3"), queued(1)).unwrap();
        deliver(&mut s, &picks, &done(1, "/home/ana/a.mp3"))
            .unwrap()
            .unwrap();
        let before = s.snapshot();

        start(&s, &picks, S, Path::new("/home/ana/b.flac"), queued(2)).unwrap();
        let failed = msg(
            2,
            "/home/ana/b.flac",
            JobEvent::Failed {
                message: "Couldn't analyse the music in \"b.flac\": unsupported audio".into(),
            },
        );
        assert!(deliver(&mut s, &picks, &failed).is_none());
        assert_eq!(s.snapshot(), before);
        // A later Done for the failed id is not applied either.
        assert!(deliver(&mut s, &picks, &done(2, "/home/ana/b.flac")).is_none());
        assert_eq!(s.snapshot(), before);
    }

    #[test]
    fn only_the_latest_pick_is_applied() {
        let mut s = session();
        let picks = MusicPicks::default();
        start(&s, &picks, S, Path::new("/home/ana/a.mp3"), queued(1)).unwrap();
        start(&s, &picks, S, Path::new("/home/ana/b.mp3"), queued(2)).unwrap();
        assert!(deliver(&mut s, &picks, &done(1, "/home/ana/a.mp3")).is_none());
        assert_eq!(s.revision(), 0);
        let r = deliver(&mut s, &picks, &done(2, "/home/ana/b.mp3")).unwrap();
        assert_eq!(
            r.unwrap().snapshot.project.sections[0]
                .music
                .as_ref()
                .unwrap()
                .path,
            "b.mp3"
        );
        // Analyses from elsewhere (project reopen jobs) are not picks.
        assert!(deliver(&mut s, &picks, &done(9, "/home/ana/c.mp3")).is_none());
    }

    #[test]
    fn bad_picks_are_refused_before_submitting() {
        let s = session();
        let picks = MusicPicks::default();
        let never = |_: Job| -> Result<JobUpdate, IpcError> { panic!("submitted") };
        let e = start(&s, &picks, S, Path::new("/home/ana/a.ogg"), never).unwrap_err();
        assert_eq!(
            e.message,
            "\"a.ogg\" is not a supported song. Use MP3, AAC, WAV or FLAC."
        );
        let e = start(&s, &picks, SectionId(9), Path::new("/home/ana/a.mp3"), never).unwrap_err();
        assert_eq!(e.message, "That part of the project no longer exists.");
        let e = start(&s, &picks, S, Path::new("relative.mp3"), never).unwrap_err();
        assert!(e.message.starts_with("\"relative.mp3\" can't be used"), "{e:?}");
        // A submit error (no job runner) comes back as is.
        let e = start(&s, &picks, S, Path::new("/home/ana/a.mp3"), |_| {
            Err(IpcError {
                message: "Video import is unavailable: no FFmpeg".into(),
            })
        })
        .unwrap_err();
        assert_eq!(e.message, "Video import is unavailable: no FFmpeg");
        assert!(picks.pending().is_empty());
    }

    #[test]
    fn music_set_on_a_removed_section_reports_an_error() {
        let mut s = session();
        let picks = MusicPicks::default();
        start(&s, &picks, S, Path::new("/home/ana/a.mp3"), queued(1)).unwrap();
        picks.pending().insert(SectionId(9), JobId(1));
        picks.pending().remove(&S);
        let e = deliver(&mut s, &picks, &done(1, "/home/ana/a.mp3"))
            .unwrap()
            .unwrap_err();
        assert_eq!(e.message, "That part of the project no longer exists.");
        assert_eq!(s.revision(), 0);
    }
}
