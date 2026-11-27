//! Camera / SD card import IPC (`ui/js/camera.js`), over
//! [`kv_session::camera`].
//!
//! - `list_cameras()` returns the mounted camera volumes
//!   ([`find_camera_volumes`] over [`default_mount_roots`]). The UI polls it
//!   (on focus and every few seconds) and shows "Import from <name>".
//! - `list_camera_recordings(volume)` returns a [`RecordingInfo`] per
//!   recording on the card, with `already_copied` set when every file is
//!   already in the library folder (same name and size; see
//!   [`is_already_copied`]).
//! - `camera_library_dir()` is the library folder recordings are copied to:
//!   `~/Movies/kwikvid` on macOS, `~/Videos/kwikvid` elsewhere (guess), or
//!   the one last chosen. `choose_camera_library_dir()` opens a folder
//!   picker and remembers the choice in `camera-import.json` in the app's
//!   config folder, next to the recent projects list; `null` when cancelled.
//! - `start_camera_import(volume, recordings, library_dir)` copies the
//!   recordings named by their first file's path with a [`CopyJob`] and
//!   returns its id; its [`CameraUpdate`]s arrive as `kv://camera` events.
//!   One import at a time. On `done` the copies (`copied`, which includes
//!   the `skipped` files already there) are imported as `library::import`
//!   does: `kv://state`, then the filmstrip jobs' `kv://job` updates, then
//!   the `done` update carrying the [`ImportReport`]. A copy that fails
//!   (card removed, disk full) keeps the finished files, imports nothing
//!   and reports `failed` with a readable message.
//! - `cancel_camera_import(id)` asks the copy to stop; `false` when it is
//!   not running.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::UNIX_EPOCH;

use kv_session::camera::date_folder;
use kv_session::{
    default_mount_roots, find_camera_volumes, list_recordings, CameraRecording, CameraVolume,
    CopyEvent, CopyJob, ImportReport,
};
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager, Runtime, State};

use crate::ipc::{AppState, IpcError};
use crate::library::{emit_job, Library};

/// The event carrying [`CameraUpdate`]s.
pub const CAMERA_EVENT: &str = "kv://camera";
/// The remembered library folder's file in the app's config folder.
pub const CONFIG_FILE: &str = "camera-import.json";
const CONFIG_VERSION: u32 = 1;
/// The date folder of a recording without a modified time (as kv-session).
const UNDATED: &str = "undated";

/// One recording on a card, as the dialog lists it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RecordingInfo {
    /// The video files, in chapter order; the first one names the
    /// recording in `start_camera_import`.
    pub files: Vec<PathBuf>,
    pub total_bytes: u64,
    /// The first file's modified time, in milliseconds since the Unix
    /// epoch (for `new Date(ms)`); `null` when unknown.
    pub modified: Option<u64>,
    /// The number of chapter files.
    pub chapters: usize,
    /// A chapter is missing on the card.
    pub has_gaps: bool,
    /// Every file is already in the library folder with the same size.
    pub already_copied: bool,
}

/// One copy event as the UI sees it (`kv://camera`): the [`CopyEvent`]
/// (tagged by `state`) and the import's id; `done` also has the `report`
/// of importing the copies.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CameraUpdate {
    pub id: u64,
    #[serde(flatten)]
    pub event: CopyEvent,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub report: Option<ImportReport>,
}

/// On-disk form of the remembered library folder.
#[derive(Serialize, Deserialize)]
struct ConfigFile {
    version: u32,
    library_dir: PathBuf,
}

/// `~/Movies/kwikvid` on macOS, `~/Videos/kwikvid` elsewhere (guess).
pub fn default_library_dir(home: &Path) -> PathBuf {
    let videos = if cfg!(target_os = "macos") {
        "Movies"
    } else {
        "Videos"
    };
    home.join(videos).join("kwikvid")
}

/// The camera volumes mounted in `roots`.
pub fn cameras(roots: &[PathBuf]) -> Vec<CameraVolume> {
    find_camera_volumes(roots)
}

/// The recordings on `volume`, with whether each is already in `library`.
pub fn recordings(volume: &Path, library: &Path) -> Vec<RecordingInfo> {
    list_recordings(volume)
        .into_iter()
        .map(|r| RecordingInfo {
            already_copied: is_already_copied(&r, library),
            modified: r
                .modified
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_millis() as u64),
            chapters: r.files.len(),
            total_bytes: r.total_bytes,
            has_gaps: r.has_gaps,
            files: r.files,
        })
        .collect()
}

/// Whether every file of `rec` is already in `library`: in its date folder
/// (as [`CopyJob`] lays it out), under its own name or a ` (N)` variant,
/// with the same size. This is the copy's own dedupe rule, so a recording
/// shown as copied is skipped by the copy.
pub fn is_already_copied(rec: &CameraRecording, library: &Path) -> bool {
    let day = rec
        .modified
        .map(date_folder)
        .unwrap_or_else(|| UNDATED.into());
    let dir = library.join(day);
    !rec.files.is_empty()
        && rec.files.iter().all(|src| match std::fs::metadata(src) {
            Ok(m) => has_copy(&dir, src, m.len()),
            Err(_) => false,
        })
}

/// Whether `dir` holds `src`'s name, or `stem (N).ext` before the first
/// free name, as a file of `size` bytes.
fn has_copy(dir: &Path, src: &Path, size: u64) -> bool {
    let name = src.file_name().unwrap_or_default();
    let stem = Path::new(name)
        .file_stem()
        .unwrap_or_default()
        .to_string_lossy();
    let ext = Path::new(name).extension().map(|e| e.to_string_lossy());
    for n in 1u32.. {
        let candidate = match (n, &ext) {
            (1, _) => dir.join(name),
            (_, Some(ext)) => dir.join(format!("{stem} ({n}).{ext}")),
            (_, None) => dir.join(format!("{stem} ({n})")),
        };
        match std::fs::metadata(&candidate) {
            Err(_) => return false,
            Ok(m) if m.is_file() && m.len() == size => return true,
            Ok(_) => {}
        }
    }
    false
}

/// The readable `failed` message for a copy from `volume` that stopped
/// with `message`.
pub fn failure_message(volume: &Path, message: &str) -> String {
    const KEPT: &str = "Files already copied were kept; nothing was imported.";
    if !volume.exists() {
        let name = volume_name(volume);
        return format!("\"{name}\" was removed during the copy. {KEPT}");
    }
    format!("{message} {KEPT}")
}

/// The volume's display name: its mount point's folder name.
fn volume_name(volume: &Path) -> String {
    volume
        .file_name()
        .unwrap_or(volume.as_os_str())
        .to_string_lossy()
        .into_owned()
}

/// A started import: its id, its copy, and whether its final update has
/// been sent (the copy thread may still be exiting).
struct Running {
    id: u64,
    job: CopyJob,
    finished: Arc<AtomicBool>,
}

impl Running {
    fn active(&self) -> bool {
        !self.finished.load(Ordering::SeqCst) && !self.job.is_finished()
    }
}

/// Tauri managed state: the library folder and the running import.
pub struct CameraImports {
    config_dir: Option<PathBuf>,
    library_dir: Mutex<PathBuf>,
    current: Mutex<Option<Running>>,
    next_id: AtomicU64,
}

impl CameraImports {
    /// The library folder remembered in `config_dir`, else `default`. A
    /// missing or damaged file is the default. `None` remembers nothing.
    pub fn load(config_dir: Option<PathBuf>, default: PathBuf) -> CameraImports {
        let saved = config_dir.as_ref().and_then(|dir| {
            let text = std::fs::read_to_string(dir.join(CONFIG_FILE)).ok()?;
            match serde_json::from_str::<ConfigFile>(&text) {
                Ok(c) if c.version == CONFIG_VERSION && c.library_dir.is_absolute() => {
                    Some(c.library_dir)
                }
                _ => {
                    tracing::warn!("{CONFIG_FILE} unreadable; using the default library folder");
                    None
                }
            }
        });
        CameraImports {
            config_dir,
            library_dir: Mutex::new(saved.unwrap_or(default)),
            current: Mutex::new(None),
            next_id: AtomicU64::new(1),
        }
    }

    /// Where recordings are copied to.
    pub fn library_dir(&self) -> PathBuf {
        lock(&self.library_dir).clone()
    }

    /// Use and remember `dir` as the library folder.
    pub fn set_library_dir(&self, dir: PathBuf) {
        let mut current = lock(&self.library_dir);
        if *current == dir {
            return;
        }
        *current = dir.clone();
        if let Some(config) = &self.config_dir {
            if let Err(e) = save_config(config, &dir) {
                tracing::warn!(error = %e, "could not remember the library folder");
            }
        }
    }

    /// Start copying the recordings of `volume` whose first file is in
    /// `chosen` into `library_dir`, and remember that folder. On `done`,
    /// `import` gets the copies; its report goes out with the `done`
    /// update, its error as `failed`. `emit` gets every update (on the
    /// copy thread). Returns the import's id.
    pub fn begin(
        &self,
        volume: &Path,
        chosen: &[PathBuf],
        library_dir: PathBuf,
        import: impl FnOnce(&[PathBuf]) -> Result<ImportReport, IpcError> + Send + 'static,
        mut emit: impl FnMut(CameraUpdate) + Send + 'static,
    ) -> Result<u64, IpcError> {
        let err = |message: String| IpcError { message };
        if chosen.is_empty() {
            return Err(err("Choose at least one recording to import.".into()));
        }
        if !library_dir.is_absolute() {
            return Err(err("Choose a folder to copy the recordings to.".into()));
        }
        let mut current = lock(&self.current);
        if current.as_ref().is_some_and(Running::active) {
            return Err(err("A camera import is already running.".into()));
        }
        let on_card = list_recordings(volume);
        let picked: Vec<CameraRecording> = on_card
            .into_iter()
            .filter(|r| r.files.first().is_some_and(|f| chosen.contains(f)))
            .collect();
        if picked.len() != chosen.len() {
            return Err(err(if volume.exists() {
                "Some recordings are no longer on the card; reopen the dialog.".into()
            } else {
                format!("\"{}\" is no longer connected.", volume_name(volume))
            }));
        }
        self.set_library_dir(library_dir.clone());
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let volume = volume.to_path_buf();
        let mut import = Some(import);
        tracing::info!(id, recordings = picked.len(), volume = %volume.display(), "camera import");
        let finished = Arc::new(AtomicBool::new(false));
        let done_flag = Arc::clone(&finished);
        let job = CopyJob::start(picked, library_dir, move |event| {
            let update = |event, report| CameraUpdate { id, event, report };
            let out = match event {
                CopyEvent::Done { copied, skipped } => {
                    let Some(import) = import.take() else { return };
                    match import(&copied) {
                        Ok(report) => update(CopyEvent::Done { copied, skipped }, Some(report)),
                        Err(e) => update(
                            CopyEvent::Failed {
                                message: format!(
                                    "The recordings were copied but couldn't be imported: {}",
                                    e.message
                                ),
                            },
                            None,
                        ),
                    }
                }
                CopyEvent::Failed { message } => update(
                    CopyEvent::Failed {
                        message: failure_message(&volume, &message),
                    },
                    None,
                ),
                other => update(other, None),
            };
            if out.event.is_final() {
                done_flag.store(true, Ordering::SeqCst);
            }
            emit(out);
        });
        *current = Some(Running { id, job, finished });
        Ok(id)
    }

    /// Cancel import `id`; `false` when it is not the running one.
    pub fn cancel(&self, id: u64) -> bool {
        match &*lock(&self.current) {
            Some(running) if running.id == id && running.active() => {
                running.job.cancel();
                true
            }
            _ => false,
        }
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    // Plain data; a panic elsewhere cannot leave it invalid.
    m.lock().unwrap_or_else(|p| p.into_inner())
}

/// Write [`CONFIG_FILE`] in `config_dir` (created if needed), atomically.
fn save_config(config_dir: &Path, library_dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(config_dir)?;
    let data = ConfigFile {
        version: CONFIG_VERSION,
        library_dir: library_dir.to_path_buf(),
    };
    let mut json = serde_json::to_vec_pretty(&data).map_err(std::io::Error::other)?;
    json.push(b'\n');
    let file = config_dir.join(CONFIG_FILE);
    let tmp = config_dir.join(format!(".{CONFIG_FILE}.tmp"));
    std::fs::write(&tmp, json)?;
    std::fs::rename(&tmp, &file)
}

// ---- Tauri commands -------------------------------------------------------

fn blocking_failed(e: impl std::fmt::Display) -> IpcError {
    IpcError {
        message: format!("Reading the card stopped unexpectedly: {e}"),
    }
}

/// The mounted camera volumes. Async: a slow card must not block the
/// window.
#[tauri::command]
pub async fn list_cameras() -> Result<Vec<CameraVolume>, IpcError> {
    tauri::async_runtime::spawn_blocking(|| cameras(&default_mount_roots()))
        .await
        .map_err(blocking_failed)
}

#[tauri::command]
pub async fn list_camera_recordings(
    app: AppHandle,
    volume: PathBuf,
) -> Result<Vec<RecordingInfo>, IpcError> {
    let library = app.state::<CameraImports>().library_dir();
    tauri::async_runtime::spawn_blocking(move || recordings(&volume, &library))
        .await
        .map_err(blocking_failed)
}

#[tauri::command]
pub fn camera_library_dir(imports: State<'_, CameraImports>) -> PathBuf {
    imports.library_dir()
}

/// A folder picker starting at the current library folder; the chosen
/// folder is remembered. `null` when cancelled.
#[tauri::command]
pub async fn choose_camera_library_dir(app: AppHandle) -> Result<Option<PathBuf>, IpcError> {
    use tauri_plugin_dialog::DialogExt as _;
    let imports = app.state::<CameraImports>();
    let current = imports.library_dir();
    let mut dialog = app.dialog().file().set_title("Copy camera recordings to");
    // The default folder may not exist yet; start in its parent then.
    if let Some(start) = current.ancestors().find(|d| d.is_dir()) {
        dialog = dialog.set_directory(start);
    }
    let Some(picked) = dialog.blocking_pick_folder() else {
        return Ok(None);
    };
    let dir = picked.into_path().map_err(|e| {
        tracing::warn!(error = %e, "library folder is not a local folder");
        IpcError {
            message: "That location can't be used for the library.".into(),
        }
    })?;
    imports.set_library_dir(dir.clone());
    Ok(Some(dir))
}

#[tauri::command]
pub fn start_camera_import(
    app: AppHandle,
    imports: State<'_, CameraImports>,
    volume: PathBuf,
    recordings: Vec<PathBuf>,
    library_dir: PathBuf,
) -> Result<u64, IpcError> {
    let worker = app.clone();
    let import = move |paths: &[PathBuf]| {
        let (report, snapshot, updates) = worker
            .state::<Library>()
            .import_paths(&worker.state::<AppState>(), paths)?;
        crate::emit_state(&worker, &snapshot);
        for u in &updates {
            emit_job(&worker, u);
        }
        Ok(report)
    };
    imports.begin(&volume, &recordings, library_dir, import, move |u| {
        emit(&app, &u)
    })
}

#[tauri::command]
pub fn cancel_camera_import(imports: State<'_, CameraImports>, id: u64) -> bool {
    imports.cancel(id)
}

fn emit<R: Runtime>(app: &AppHandle<R>, update: &CameraUpdate) {
    if let Err(e) = app.emit(CAMERA_EVENT, update) {
        tracing::warn!(error = %e, "could not emit {CAMERA_EVENT}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kv_session::Session;
    use std::sync::mpsc;
    use std::time::Duration;

    /// A fake card `<root>/GOPRO` with recording 42 (two chapters, plus
    /// an LRV proxy) and recording 43 (one chapter).
    struct Card {
        _dir: tempfile::TempDir,
        root: PathBuf,
        volume: PathBuf,
        library: PathBuf,
    }

    fn card() -> Card {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("mnt");
        let volume = root.join("GOPRO");
        let folder = volume.join("DCIM/100GOPRO");
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::create_dir_all(root.join("Macintosh HD")).unwrap();
        for (name, len) in [
            ("GX010042.MP4", 3000),
            ("GX020042.MP4", 2000),
            ("GX010042.LRV", 100),
            ("GX010043.MP4", 500),
        ] {
            std::fs::write(folder.join(name), vec![7u8; len]).unwrap();
        }
        let library = dir.path().join("Movies/kwikvid");
        Card {
            root,
            volume,
            library,
            _dir: dir,
        }
    }

    fn first_files(recs: &[RecordingInfo]) -> Vec<PathBuf> {
        recs.iter().map(|r| r.files[0].clone()).collect()
    }

    /// The updates of one import, until its final one.
    fn collect(rx: &mpsc::Receiver<CameraUpdate>) -> Vec<CameraUpdate> {
        let mut out = Vec::new();
        loop {
            let u = rx.recv_timeout(Duration::from_secs(10)).expect("update");
            let last = u.event.is_final();
            out.push(u);
            if last {
                return out;
            }
        }
    }

    fn sender() -> (
        impl FnMut(CameraUpdate) + Send + 'static,
        mpsc::Receiver<CameraUpdate>,
    ) {
        let (tx, rx) = mpsc::channel();
        (
            move |u| {
                let _ = tx.send(u);
            },
            rx,
        )
    }

    fn imports(card: &Card) -> CameraImports {
        CameraImports::load(None, card.library.clone())
    }

    #[test]
    fn lists_cameras_and_recordings_with_chapters() {
        let c = card();
        let vols = cameras(std::slice::from_ref(&c.root));
        assert_eq!(vols.len(), 1);
        assert_eq!(vols[0].name, "GOPRO");
        assert!(vols[0].is_gopro);
        assert!(cameras(&[c.root.join("nothing")]).is_empty());

        let recs = recordings(&c.volume, &c.library);
        assert_eq!(recs.len(), 2);
        assert_eq!(recs[0].chapters, 2);
        assert_eq!(recs[0].total_bytes, 5000);
        assert!(recs[0].files[0].ends_with("GX010042.MP4"));
        assert!(recs[0].files[1].ends_with("GX020042.MP4"));
        assert_eq!(recs[1].chapters, 1);
        assert_eq!(recs[1].total_bytes, 500);
        assert!(recs[0].modified.is_some_and(|ms| ms > 1_600_000_000_000));
        assert!(recs.iter().all(|r| !r.already_copied && !r.has_gaps));
    }

    #[test]
    fn copied_recordings_are_marked_and_only_when_complete_with_same_size() {
        let c = card();
        let recs = recordings(&c.volume, &c.library);
        let rec = list_recordings(&c.volume).remove(0);
        let day = c.library.join(date_folder(rec.modified.unwrap()));
        std::fs::create_dir_all(&day).unwrap();
        // One chapter only: not copied.
        std::fs::write(day.join("GX010042.MP4"), vec![7u8; 3000]).unwrap();
        assert!(!is_already_copied(&rec, &c.library));
        // Second chapter of another size: not copied...
        std::fs::write(day.join("GX020042.MP4"), vec![7u8; 10]).unwrap();
        assert!(!is_already_copied(&rec, &c.library));
        // ...unless its copy went to the " (2)" name.
        std::fs::write(day.join("GX020042 (2).MP4"), vec![7u8; 2000]).unwrap();
        assert!(is_already_copied(&rec, &c.library));
        let again = recordings(&c.volume, &c.library);
        assert!(again[0].already_copied && !again[1].already_copied);
        assert_eq!(first_files(&again), first_files(&recs));
    }

    /// The whole flow without Tauri: copy two recordings, import them
    /// (filmstrips queued), then they list as already copied and a second
    /// import skips them.
    #[test]
    fn copy_then_import_through_the_library() {
        let c = card();
        let state = std::sync::Arc::new(AppState::new(Session::new(c.root.clone())));
        let (library, _events) = Library::new(crate::library::tests::ctx());
        let library = std::sync::Arc::new(library);
        let imports = imports(&c);
        let recs = recordings(&c.volume, &c.library);
        let (emit, rx) = sender();
        let (jobs_tx, jobs_rx) = mpsc::channel();
        let (st, lib) = (state.clone(), library.clone());
        let id = imports
            .begin(
                &c.volume,
                &first_files(&recs),
                c.library.clone(),
                move |paths| {
                    let (report, _snapshot, updates) = lib.import_paths(&st, paths)?;
                    jobs_tx.send(updates).unwrap();
                    Ok(report)
                },
                emit,
            )
            .unwrap();
        let updates = collect(&rx);
        assert!(updates.iter().all(|u| u.id == id));
        assert!(matches!(
            updates[0].event,
            CopyEvent::Started {
                total_bytes: 5500,
                files: 3
            }
        ));
        let progress: Vec<u64> = updates
            .iter()
            .filter_map(|u| match u.event {
                CopyEvent::Progress { bytes_done, .. } => Some(bytes_done),
                _ => None,
            })
            .collect();
        assert_eq!(progress.last(), Some(&5500));
        let last = updates.last().unwrap();
        let CopyEvent::Done { copied, skipped } = &last.event else {
            panic!("not done: {last:?}");
        };
        assert_eq!(copied.len(), 3);
        assert!(skipped.is_empty());
        assert!(copied
            .iter()
            .all(|p| p.starts_with(&c.library) && p.is_file()));
        // Chapters import as one item each.
        let report = last.report.as_ref().unwrap();
        assert_eq!(report.added.len(), 2);
        assert_eq!(state.session().unwrap().project().media.len(), 2);
        let queued = jobs_rx.recv().unwrap();
        assert_eq!(queued.len(), 2);
        assert!(queued
            .iter()
            .all(|u| u.kind == "filmstrip" && u.state == "queued"));
        assert!(!imports.cancel(id), "a finished import cannot be cancelled");

        let again = recordings(&c.volume, &c.library);
        assert!(again.iter().all(|r| r.already_copied));
        // A second import copies nothing and still imports the copies.
        let (emit, rx) = sender();
        let mut seen = Vec::new();
        let (tx, paths_rx) = mpsc::channel();
        imports
            .begin(
                &c.volume,
                &first_files(&again)[..1],
                c.library.clone(),
                move |paths| {
                    tx.send(paths.to_vec()).unwrap();
                    Ok(ImportReport::default())
                },
                emit,
            )
            .unwrap();
        seen.extend(collect(&rx));
        let CopyEvent::Done { copied, skipped } = &seen.last().unwrap().event else {
            panic!("not done");
        };
        assert_eq!(copied.len(), 2);
        assert_eq!(skipped, copied);
        assert_eq!(&paths_rx.recv().unwrap(), copied);
    }

    #[test]
    fn rejects_empty_unknown_and_concurrent_imports() {
        let c = card();
        let imports = imports(&c);
        let recs = recordings(&c.volume, &c.library);
        let ok = |_: &[PathBuf]| Ok(ImportReport::default());
        let e = imports
            .begin(&c.volume, &[], c.library.clone(), ok, |_| {})
            .unwrap_err();
        assert_eq!(e.message, "Choose at least one recording to import.");
        let e = imports
            .begin(
                &c.volume,
                &[c.volume.join("DCIM/100GOPRO/GX010099.MP4")],
                c.library.clone(),
                ok,
                |_| {},
            )
            .unwrap_err();
        assert!(e.message.contains("no longer on the card"), "{e:?}");
        let e = imports
            .begin(&c.volume, &first_files(&recs), "rel".into(), ok, |_| {})
            .unwrap_err();
        assert_eq!(e.message, "Choose a folder to copy the recordings to.");

        // The first import is held in its import step: still running.
        let (release_tx, release) = mpsc::channel::<()>();
        let (entered_tx, entered) = mpsc::channel();
        let (emit, rx) = sender();
        let id = imports
            .begin(
                &c.volume,
                &first_files(&recs),
                c.library.clone(),
                move |_| {
                    entered_tx.send(()).unwrap();
                    release.recv().unwrap();
                    Ok(ImportReport::default())
                },
                emit,
            )
            .unwrap();
        entered.recv_timeout(Duration::from_secs(10)).unwrap();
        let e = imports
            .begin(
                &c.volume,
                &first_files(&recs),
                c.library.clone(),
                ok,
                |_| {},
            )
            .unwrap_err();
        assert_eq!(e.message, "A camera import is already running.");
        assert!(!imports.cancel(id + 1));
        release_tx.send(()).unwrap();
        assert!(collect(&rx).last().unwrap().report.is_some());
    }

    #[test]
    fn cancel_stops_the_copy_and_imports_nothing() {
        let c = card();
        let big = c.volume.join("DCIM/100GOPRO/GX010044.MP4");
        // Sparse: cheap to create, 64 MiB to copy.
        std::fs::File::create(&big)
            .unwrap()
            .set_len(64 << 20)
            .unwrap();
        let imports = imports(&c);
        let (emit, rx) = sender();
        let id = imports
            .begin(
                &c.volume,
                std::slice::from_ref(&big),
                c.library.clone(),
                |_| panic!("nothing should be imported"),
                emit,
            )
            .unwrap();
        assert!(imports.cancel(id));
        let updates = collect(&rx);
        assert_eq!(updates.last().unwrap().event, CopyEvent::Cancelled);
        let name = big.file_name().unwrap();
        let leftover = walk(&c.library).into_iter().any(|p| {
            p.file_name()
                .is_some_and(|n| n.to_string_lossy().contains(&*name.to_string_lossy()))
        });
        assert!(
            !leftover,
            "no partial or finished file of the cancelled copy"
        );
    }

    fn walk(dir: &Path) -> Vec<PathBuf> {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return vec![];
        };
        entries
            .flatten()
            .flat_map(|e| {
                let p = e.path();
                if p.is_dir() {
                    walk(&p)
                } else {
                    vec![p]
                }
            })
            .collect()
    }

    /// The card is removed mid-copy: `failed` with a readable message, the
    /// chapter already copied kept, no partial file, nothing imported.
    #[test]
    fn card_removed_mid_copy_keeps_finished_files_and_imports_nothing() {
        let c = card();
        // A large (sparse) first chapter, so the card can be removed while
        // it is being copied.
        let first = c.volume.join("DCIM/100GOPRO/GX010042.MP4");
        std::fs::File::create(&first)
            .unwrap()
            .set_len(64 << 20)
            .unwrap();
        let imports = imports(&c);
        let recs = recordings(&c.volume, &c.library);
        assert_eq!(recs[0].chapters, 2);
        let (emit, rx) = sender();
        imports
            .begin(
                &c.volume,
                &first_files(&recs)[..1],
                c.library.clone(),
                |_| panic!("nothing should be imported"),
                emit,
            )
            .unwrap();
        // Once the first chapter is being copied (its file stays readable
        // while open), the card goes away.
        loop {
            let u = rx.recv_timeout(Duration::from_secs(10)).unwrap();
            assert!(!u.event.is_final(), "finished too early: {u:?}");
            if matches!(u.event, CopyEvent::Progress { .. }) {
                break;
            }
        }
        std::fs::remove_dir_all(&c.volume).unwrap();
        let last = collect(&rx).pop().unwrap();
        let CopyEvent::Failed { message } = &last.event else {
            panic!("not failed: {last:?}");
        };
        assert_eq!(
            message,
            "\"GOPRO\" was removed during the copy. \
             Files already copied were kept; nothing was imported."
        );
        let kept = walk(&c.library);
        assert_eq!(kept.len(), 1, "{kept:?}");
        assert!(kept[0].ends_with("GX010042.MP4"));
    }

    /// The card is gone when the copy fails: the message says so. Starting
    /// an import from a card that is gone fails before copying.
    #[test]
    fn removed_card_says_so() {
        let c = card();
        let imports = imports(&c);
        let chosen = first_files(&recordings(&c.volume, &c.library));
        let read_error = "Couldn't read \"GX010042.MP4\": gone.";
        assert_eq!(
            failure_message(&c.volume, read_error),
            "Couldn't read \"GX010042.MP4\": gone. \
             Files already copied were kept; nothing was imported."
        );
        std::fs::remove_dir_all(&c.volume).unwrap();
        assert_eq!(
            failure_message(&c.volume, read_error),
            "\"GOPRO\" was removed during the copy. \
             Files already copied were kept; nothing was imported."
        );
        let e = imports
            .begin(
                &c.volume,
                &chosen,
                c.library.clone(),
                |_| unreachable!(),
                |_| {},
            )
            .unwrap_err();
        assert_eq!(e.message, "\"GOPRO\" is no longer connected.");
        assert!(walk(&c.library).is_empty());
    }

    #[test]
    fn library_dir_defaults_and_is_remembered() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let default = default_library_dir(&home);
        if cfg!(target_os = "macos") {
            assert_eq!(default, home.join("Movies/kwikvid"));
        } else {
            assert_eq!(default, home.join("Videos/kwikvid"));
        }
        let config = dir.path().join("config");
        let a = CameraImports::load(Some(config.clone()), default.clone());
        assert_eq!(a.library_dir(), default);
        let chosen = dir.path().join("Footage");
        a.set_library_dir(chosen.clone());
        assert_eq!(a.library_dir(), chosen);
        assert!(config.join(CONFIG_FILE).is_file());
        let b = CameraImports::load(Some(config.clone()), default.clone());
        assert_eq!(b.library_dir(), chosen);
        // A damaged file falls back to the default.
        std::fs::write(config.join(CONFIG_FILE), "{nope").unwrap();
        let c = CameraImports::load(Some(config), default.clone());
        assert_eq!(c.library_dir(), default);
    }

    #[test]
    fn updates_and_recordings_serialise_for_the_ui() {
        let done = CameraUpdate {
            id: 3,
            event: CopyEvent::Done {
                copied: vec!["/l/a.mp4".into()],
                skipped: vec![],
            },
            report: Some(ImportReport::default()),
        };
        assert_eq!(
            serde_json::to_value(&done).unwrap(),
            serde_json::json!({"id": 3, "state": "done", "copied": ["/l/a.mp4"], "skipped": [],
                "report": {"added": [], "skipped": []}})
        );
        let progress = CameraUpdate {
            id: 3,
            event: CopyEvent::Progress {
                bytes_done: 1,
                total_bytes: 2,
                file: "/c/a.mp4".into(),
            },
            report: None,
        };
        assert_eq!(
            serde_json::to_value(&progress).unwrap(),
            serde_json::json!({"id": 3, "state": "progress", "bytes_done": 1, "total_bytes": 2,
                "file": "/c/a.mp4"})
        );
    }

    /// The UI fixtures (written by `tests/ui_fixtures.rs`, which cannot
    /// import this module) have the keys of the real types.
    #[test]
    fn ui_fixtures_have_the_real_keys() {
        let fixture = |name: &str| -> serde_json::Value {
            let path = Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("ui-tests/fixtures")
                .join(format!("{name}.json"));
            serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
        };
        let keys = |v: &serde_json::Value| {
            let mut k: Vec<String> = v.as_object().unwrap().keys().cloned().collect();
            k.sort();
            k
        };
        let c = card();
        let real = serde_json::to_value(&recordings(&c.volume, &c.library)[0]).unwrap();
        let recs = fixture("camera-recordings");
        for r in recs.as_array().unwrap() {
            assert_eq!(keys(r), keys(&real));
        }
        let vols = fixture("camera-volumes");
        let real = serde_json::to_value(&cameras(std::slice::from_ref(&c.root))[0]).unwrap();
        assert_eq!(keys(&vols[0]), keys(&real));
        let done = fixture("camera-done");
        let real = serde_json::to_value(CameraUpdate {
            id: 1,
            event: CopyEvent::Done {
                copied: vec![],
                skipped: vec![],
            },
            report: Some(ImportReport::default()),
        })
        .unwrap();
        assert_eq!(keys(&done), keys(&real));
    }
}
