//! Project files from the desktop app: New, Open, Save, Save As, the recent
//! projects list and the quit check.
//!
//! The handlers are plain functions over [`AppState`] and [`ProjectFiles`],
//! testable without a window; the `#[tauri::command]` wrappers below add the
//! window side (native file pickers, `kv://state`, rebuilding the Open
//! Recent menu). The UI (`ui/js/project.js`) runs the flows: it asks about
//! unsaved changes, picks a path, then calls one of these commands. Every
//! one returns a [`ProjectState`].

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use kv_session::{MissingFile, ProjectFileError, RecentProjects, Session, Snapshot};
use serde::Serialize;
use tauri::{AppHandle, Manager, Runtime, State};

use crate::ipc::{AppState, IpcError};
use crate::menu;

/// What every project command returns to the UI.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProjectState {
    pub snapshot: Snapshot,
    /// The project file; `null` while untitled. The UI shows its name in the
    /// window title.
    pub file: Option<PathBuf>,
    /// Media and music files of a just-opened project that do not exist.
    /// Empty for every command but open.
    pub missing: Vec<MissingFile>,
}

impl From<ProjectFileError> for IpcError {
    fn from(e: ProjectFileError) -> IpcError {
        tracing::warn!(error = ?e, "project file action failed");
        IpcError {
            message: e.to_string(),
        }
    }
}

/// Tauri managed state next to [`AppState`]: the recent projects list
/// (persisted in the app's config folder) and whether quitting with unsaved
/// changes was confirmed.
pub struct ProjectFiles {
    config_dir: Option<PathBuf>,
    recent: Mutex<RecentProjects>,
    quit_confirmed: AtomicBool,
}

impl ProjectFiles {
    /// Load the recent list from `config_dir`; a missing or damaged list is
    /// an empty one. `None` keeps the list in memory only.
    pub fn load(config_dir: Option<PathBuf>) -> ProjectFiles {
        let recent = match &config_dir {
            Some(dir) => RecentProjects::load(dir).unwrap_or_else(|e| {
                tracing::warn!(error = ?e, "recent projects list unreadable; starting empty");
                RecentProjects::default()
            }),
            None => RecentProjects::default(),
        };
        ProjectFiles {
            config_dir,
            recent: Mutex::new(recent),
            quit_confirmed: AtomicBool::new(false),
        }
    }

    /// The recent projects, newest first.
    pub fn recent(&self) -> Vec<PathBuf> {
        self.lock().paths().to_vec()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, RecentProjects> {
        // The list is plain data; a panic elsewhere cannot leave it invalid.
        self.recent.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn update(&self, change: impl FnOnce(&mut RecentProjects)) {
        let mut list = self.lock();
        change(&mut list);
        if let Some(dir) = &self.config_dir {
            if let Err(e) = list.save(dir) {
                tracing::warn!(error = ?e, "could not save the recent projects list");
            }
        }
    }

    /// Whether quitting should ask about unsaved changes first.
    pub fn needs_quit_prompt(&self, state: &AppState) -> bool {
        if self.quit_confirmed.load(Ordering::SeqCst) {
            return false;
        }
        // A broken session cannot be saved anyway; let the app quit.
        state.session().map(|s| s.is_dirty()).unwrap_or(false)
    }

    /// The UI has asked about unsaved changes; quit without asking again.
    pub fn confirm_quit(&self) {
        self.quit_confirmed.store(true, Ordering::SeqCst);
    }
}

fn project_state(session: &Session, missing: Vec<MissingFile>) -> ProjectState {
    ProjectState {
        snapshot: session.snapshot(),
        file: session.file_path().map(Path::to_path_buf),
        missing,
    }
}

/// The current project, unchanged.
pub fn info(state: &AppState) -> Result<ProjectState, IpcError> {
    let session = state.session()?;
    Ok(project_state(&session, Vec::new()))
}

/// Replace the project with a new, untitled one whose media paths are
/// relative to `media_root`. Unsaved changes are discarded (the UI asks
/// first).
pub fn start_new(state: &AppState, media_root: PathBuf) -> Result<ProjectState, IpcError> {
    let mut session = state.session()?;
    let mut fresh = Session::new(media_root);
    fresh.continue_revisions(session.revision());
    *session = fresh;
    tracing::info!("new project");
    Ok(project_state(&session, Vec::new()))
}

/// Open a project file in place of the current project and put it at the
/// top of the recent list. The project opens even when media files are
/// missing; they are listed in [`ProjectState::missing`]. A file that no
/// longer exists is dropped from the recent list.
pub fn open(
    state: &AppState,
    files: &ProjectFiles,
    path: &Path,
) -> Result<ProjectState, IpcError> {
    let mut session = state.session()?;
    let (mut opened, missing) = match Session::open(path) {
        Ok(r) => r,
        Err(e) => {
            if matches!(&e, ProjectFileError::Io { source, .. } if source.kind() == std::io::ErrorKind::NotFound)
            {
                files.update(|r| r.remove(path));
            }
            return Err(e.into());
        }
    };
    opened.continue_revisions(session.revision());
    *session = opened;
    if let Some(file) = session.file_path() {
        files.update(|r| r.add(file));
    }
    Ok(project_state(&session, missing))
}

/// Save to the project's file. Fails with "Choose where to save the
/// project first." while untitled (the UI calls [`save_as`] instead).
pub fn save(state: &AppState) -> Result<ProjectState, IpcError> {
    let mut session = state.session()?;
    session.save()?;
    Ok(project_state(&session, Vec::new()))
}

/// Save to `path` (`.kwikvid` appended if needed), make it the project's
/// file and put it at the top of the recent list.
pub fn save_as(
    state: &AppState,
    files: &ProjectFiles,
    path: &Path,
) -> Result<ProjectState, IpcError> {
    let mut session = state.session()?;
    session.save_as(path)?;
    if let Some(file) = session.file_path() {
        files.update(|r| r.add(file));
    }
    Ok(project_state(&session, Vec::new()))
}

// ---- Tauri commands -------------------------------------------------------

fn changed<R: Runtime>(
    app: &AppHandle<R>,
    result: Result<ProjectState, IpcError>,
) -> Result<ProjectState, IpcError> {
    if let Ok(p) = &result {
        crate::emit_state(app, &p.snapshot);
    }
    result
}

fn refresh_recent<R: Runtime>(app: &AppHandle<R>) {
    menu::refresh(app, &app.state::<ProjectFiles>().recent());
}

#[tauri::command]
pub fn project_info(state: State<'_, AppState>) -> Result<ProjectState, IpcError> {
    info(&state)
}

#[tauri::command]
pub fn new_project(app: AppHandle, state: State<'_, AppState>) -> Result<ProjectState, IpcError> {
    changed(&app, start_new(&state, crate::media_root(&app)))
}

#[tauri::command]
pub fn open_project(
    app: AppHandle,
    state: State<'_, AppState>,
    files: State<'_, ProjectFiles>,
    path: PathBuf,
) -> Result<ProjectState, IpcError> {
    let result = changed(&app, open(&state, &files, &path));
    refresh_recent(&app);
    result
}

#[tauri::command]
pub fn save_project(app: AppHandle, state: State<'_, AppState>) -> Result<ProjectState, IpcError> {
    changed(&app, save(&state))
}

#[tauri::command]
pub fn save_project_as(
    app: AppHandle,
    state: State<'_, AppState>,
    files: State<'_, ProjectFiles>,
    path: PathBuf,
) -> Result<ProjectState, IpcError> {
    let result = changed(&app, save_as(&state, &files, &path));
    if result.is_ok() {
        refresh_recent(&app);
    }
    result
}

const FILTER_NAME: &str = "kwikvid project";

fn picked(path: Option<tauri_plugin_dialog::FilePath>) -> Result<Option<PathBuf>, IpcError> {
    path.map(|p| {
        p.into_path().map_err(|e| {
            tracing::warn!(error = %e, "picked path is not a local file");
            IpcError {
                message: "That location can't be used for a project.".into(),
            }
        })
    })
    .transpose()
}

/// The native Open dialog; `null` when cancelled. Async so the blocking
/// dialog runs off the main thread.
#[tauri::command]
pub async fn choose_open_path(app: AppHandle) -> Result<Option<PathBuf>, IpcError> {
    use tauri_plugin_dialog::DialogExt as _;
    picked(
        app.dialog()
            .file()
            .add_filter(FILTER_NAME, &[kv_session::PROJECT_EXTENSION])
            .blocking_pick_file(),
    )
}

/// The native Save dialog, starting in the project's folder with its name
/// (or "Untitled"); `null` when cancelled.
#[tauri::command]
pub async fn choose_save_path(
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<Option<PathBuf>, IpcError> {
    use tauri_plugin_dialog::DialogExt as _;
    let (dir, name) = {
        let s = state.session()?;
        let name = s
            .file_path()
            .and_then(|f| f.file_stem())
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "Untitled".into());
        (s.media_root().to_path_buf(), name)
    };
    picked(
        app.dialog()
            .file()
            .add_filter(FILTER_NAME, &[kv_session::PROJECT_EXTENSION])
            .set_directory(dir)
            .set_file_name(format!("{name}.{}", kv_session::PROJECT_EXTENSION))
            .blocking_save_file(),
    )
}

/// Set the native window title (the UI computes it: project name plus an
/// edited marker).
#[tauri::command]
pub fn set_window_title(window: tauri::WebviewWindow, title: String) -> Result<(), IpcError> {
    window.set_title(&title).map_err(|e| {
        tracing::warn!(error = %e, "could not set the window title");
        IpcError {
            message: "The window title couldn't be updated.".into(),
        }
    })
}

/// Quit now; the UI has already asked about unsaved changes.
#[tauri::command]
pub fn quit(app: AppHandle, files: State<'_, ProjectFiles>) {
    files.confirm_quit();
    app.exit(0);
}

#[cfg(test)]
mod tests {
    use super::*;
    use kv_core::{Command, MediaId, MediaItem, MediaKind, Micros, Project};

    fn clip(id: u64, path: &str) -> Command {
        Command::AddMedia {
            item: MediaItem {
                id: MediaId(id),
                path: path.into(),
                duration: Micros(10_000_000),
                kind: MediaKind::Video,
                hilights: vec![],
                clip_audio: false,
                chapters: vec![],
            },
        }
    }

    fn setup() -> (tempfile::TempDir, AppState, ProjectFiles) {
        let dir = tempfile::tempdir().unwrap();
        let state = AppState::new(Session::new(dir.path().to_path_buf()));
        let files = ProjectFiles::load(Some(dir.path().join("config")));
        (dir, state, files)
    }

    fn edit(state: &AppState) {
        crate::ipc::apply(
            state,
            Command::SetSeed {
                section: Project::DEFAULT_SECTION,
                seed: 9,
            },
        )
        .unwrap();
    }

    #[test]
    fn untitled_save_asks_for_a_location() {
        let (_d, state, _f) = setup();
        let e = save(&state).unwrap_err();
        assert_eq!(e.message, "Choose where to save the project first.");
        assert_eq!(info(&state).unwrap().file, None);
    }

    #[test]
    fn save_as_then_save_clears_dirty_and_records_recent() {
        let (dir, state, files) = setup();
        edit(&state);
        assert!(info(&state).unwrap().snapshot.dirty);

        let p = save_as(&state, &files, &dir.path().join("trip")).unwrap();
        let file = dir.path().join("trip.kwikvid");
        assert_eq!(p.file.as_deref(), Some(file.as_path()));
        assert!(!p.snapshot.dirty);
        assert!(file.exists());
        assert_eq!(files.recent(), vec![file.clone()]);

        edit(&state);
        crate::ipc::undo(&state).unwrap();
        crate::ipc::undo(&state).unwrap();
        assert!(info(&state).unwrap().snapshot.dirty);
        let s = save(&state).unwrap();
        assert!(!s.snapshot.dirty);
        assert_eq!(s.file.as_deref(), Some(file.as_path()));

        // The recent list survives a restart.
        let again = ProjectFiles::load(Some(dir.path().join("config")));
        assert_eq!(again.recent(), vec![file]);
    }

    #[test]
    fn open_replaces_the_project_and_reports_missing_media() {
        let (dir, state, files) = setup();
        // A project with two clips, one of which exists.
        std::fs::write(dir.path().join("a.mp4"), b"x").unwrap();
        let mut other = Session::new(dir.path().to_path_buf());
        other.apply(clip(1, "a.mp4")).unwrap();
        other.apply(clip(2, "clips/b.mp4")).unwrap();
        let file = dir.path().join("trip.kwikvid");
        other.save_as(&file).unwrap();

        edit(&state);
        let before = info(&state).unwrap().snapshot.revision;
        let p = open(&state, &files, &file).unwrap();
        assert_eq!(p.file.as_deref(), Some(file.as_path()));
        assert_eq!(p.snapshot.project.media.len(), 2);
        assert!(!p.snapshot.dirty && !p.snapshot.can_undo);
        // Revisions keep increasing, so the UI does not drop the new state.
        assert_eq!(p.snapshot.revision, before + 1);
        assert_eq!(p.missing.len(), 1);
        assert_eq!(p.missing[0].project_path, "clips/b.mp4");
        assert_eq!(files.recent(), vec![file]);
        assert_eq!(info(&state).unwrap().snapshot, p.snapshot);
    }

    #[test]
    fn opening_a_vanished_recent_file_drops_it() {
        let (dir, state, files) = setup();
        let file = dir.path().join("gone.kwikvid");
        files.update(|r| r.add(&file));
        let e = open(&state, &files, &file).unwrap_err();
        assert_eq!(e.message, "The project file couldn't be read or written.");
        assert!(files.recent().is_empty());
        assert_eq!(info(&state).unwrap().snapshot.revision, 0);
    }

    #[test]
    fn new_project_is_untitled_clean_and_newer() {
        let (dir, state, files) = setup();
        edit(&state);
        save_as(&state, &files, &dir.path().join("trip.kwikvid")).unwrap();
        edit(&state);
        let before = info(&state).unwrap().snapshot.revision;
        let p = start_new(&state, PathBuf::from("/home/ana")).unwrap();
        assert_eq!(p.file, None);
        assert!(!p.snapshot.dirty && !p.snapshot.can_undo);
        assert_eq!(p.snapshot.revision, before + 1);
        assert_eq!(p.snapshot.media_root, PathBuf::from("/home/ana"));
    }

    #[test]
    fn quit_prompt_only_with_unsaved_changes() {
        let (_d, state, files) = setup();
        assert!(!files.needs_quit_prompt(&state));
        edit(&state);
        assert!(files.needs_quit_prompt(&state));
        files.confirm_quit();
        assert!(!files.needs_quit_prompt(&state));
    }

    #[test]
    fn project_state_serialises_for_the_ui() {
        let (_d, state, _f) = setup();
        let v = serde_json::to_value(info(&state).unwrap()).unwrap();
        assert_eq!(v["file"], serde_json::Value::Null);
        assert_eq!(v["missing"], serde_json::json!([]));
        assert_eq!(v["snapshot"]["revision"], 0);
    }

    #[test]
    fn missing_file_json_shape() {
        let m = MissingFile {
            kind: kv_session::MissingKind::Media {
                media_id: MediaId(3),
            },
            project_path: "clips/c.mp4".into(),
            resolved: PathBuf::from("/p/clips/c.mp4"),
        };
        assert_eq!(
            serde_json::to_value(m).unwrap(),
            serde_json::json!({
                "kind": {"type": "media", "media_id": 3},
                "project_path": "clips/c.mp4",
                "resolved": "/p/clips/c.mp4"
            })
        );
    }
}
