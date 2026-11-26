//! Export dialog IPC (`ui/js/export.js`), over [`kv_session::export`].
//!
//! - `export_options(section)` returns the dialog's [`ExportOptions`]: the
//!   codecs this Mac can encode (HEVC only where the encoder supports it),
//!   the project's aspect ratio, `<project name>.mp4`, and `blocked`, the
//!   reason exporting is not possible (empty edit, missing media, no
//!   FFmpeg).
//! - `choose_export_path(name)` opens the native Save dialog for file
//!   `name` in the Movies folder (the home folder where there is none);
//!   `null` when cancelled.
//! - `start_export(section, request)` starts an [`ExportJob`] with an
//!   [`ExportRequest`] and returns its id; its [`ExportUpdate`]s arrive as
//!   `kv://export` events. One export at a time.
//! - `cancel_export(id)` asks it to stop (it then reports `"cancelled"` and
//!   leaves no file); `false` when it is not running.
//! - `reveal_export(path)` shows the file in Finder (opener plugin).
//!
//! Changing the aspect ratio in the dialog is a plain
//! `Command::SetAspectRatio` through `apply`, so the preview follows.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use kv_core::SectionId;
use kv_media::{Encoder, FfmpegBackend, MediaBackend};
use kv_session::{ExportJob, ExportOptions, ExportRequest, ExportUpdate};
use tauri::{AppHandle, Emitter, Manager, Runtime, State};

use crate::ipc::{AppState, IpcError};

/// The event carrying [`ExportUpdate`]s.
pub const EXPORT_EVENT: &str = "kv://export";

/// The decoder and encoder exports use, or why there are none.
pub type Backend = Result<(Arc<dyn MediaBackend>, Arc<dyn Encoder>), String>;

/// Tauri managed state: the encoder and the running export.
pub struct Exports {
    backend: Backend,
    current: Mutex<Option<(u64, ExportJob)>>,
    next_id: AtomicU64,
}

impl Exports {
    /// Exports through FFmpeg, or none (with the reason) when it can't start.
    pub fn start() -> Exports {
        let backend = FfmpegBackend::new()
            .map(|b| {
                let b = Arc::new(b);
                (b.clone() as Arc<dyn MediaBackend>, b as Arc<dyn Encoder>)
            })
            .map_err(|e| format!("Export is unavailable: {e}"));
        Exports::with_backend(backend)
    }

    pub fn with_backend(backend: Backend) -> Exports {
        Exports {
            backend,
            current: Mutex::new(None),
            next_id: AtomicU64::new(1),
        }
    }

    fn current(&self) -> MutexGuard<'_, Option<(u64, ExportJob)>> {
        self.current.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The dialog's options for `section`.
    pub fn options(&self, state: &AppState, section: SectionId) -> Result<ExportOptions, IpcError> {
        let session = state.session()?;
        match &self.backend {
            Ok((_, encoder)) => Ok(session.export_options(section, &**encoder)),
            Err(why) => {
                struct NoEncoder;
                impl Encoder for NoEncoder {
                    fn create(
                        &self,
                        _: &std::path::Path,
                        _: &kv_media::ExportSettings,
                    ) -> kv_media::Result<Box<dyn kv_media::VideoWriter>> {
                        Err(kv_media::MediaError::Unsupported("no encoder".into()))
                    }
                    fn supports(&self, _: kv_media::VideoCodec) -> bool {
                        false
                    }
                }
                let mut o = session.export_options(section, &NoEncoder);
                o.codecs = vec![kv_media::VideoCodec::H264];
                o.blocked = Some(why.clone());
                Ok(o)
            }
        }
    }

    /// Start exporting; `emit` gets every update (on the export thread).
    pub fn begin(
        &self,
        state: &AppState,
        section: SectionId,
        request: ExportRequest,
        mut emit: impl FnMut(ExportUpdate) + Send + 'static,
    ) -> Result<u64, IpcError> {
        let err = |message: String| IpcError { message };
        let (media, encoder) = self.backend.clone().map_err(err)?;
        if !encoder.supports(request.codec) {
            return Err(err("This Mac can't encode HEVC; choose H.264.".into()));
        }
        let mut current = self.current();
        if current.as_ref().is_some_and(|(_, job)| !job.is_finished()) {
            return Err(err("An export is already running.".into()));
        }
        let input = state
            .session()?
            .export_input(section, &request)
            .map_err(err)?;
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let job = ExportJob::start(input, media, encoder, move |event| {
            emit(ExportUpdate { id, event })
        });
        *current = Some((id, job));
        Ok(id)
    }

    /// Cancel export `id`; `false` when it is not the running one.
    pub fn cancel(&self, id: u64) -> bool {
        match &*self.current() {
            Some((running, job)) if *running == id && !job.is_finished() => {
                job.cancel();
                true
            }
            _ => false,
        }
    }
}

#[tauri::command]
pub fn export_options(
    state: State<'_, AppState>,
    exports: State<'_, Exports>,
    section: SectionId,
) -> Result<ExportOptions, IpcError> {
    exports.options(&state, section)
}

/// The native Save dialog for the exported file. Async so the blocking
/// dialog runs off the main thread.
#[tauri::command]
pub async fn choose_export_path(app: AppHandle, name: String) -> Result<Option<PathBuf>, IpcError> {
    use tauri_plugin_dialog::DialogExt as _;
    let dir = app.path().video_dir().or_else(|_| app.path().home_dir());
    let mut dialog = app
        .dialog()
        .file()
        .add_filter("MPEG-4 video", &["mp4"])
        .set_file_name(name);
    if let Ok(dir) = dir {
        dialog = dialog.set_directory(dir);
    }
    dialog
        .blocking_save_file()
        .map(|p| {
            p.into_path().map_err(|e| {
                tracing::warn!(error = %e, "export path is not a local file");
                IpcError {
                    message: "That location can't be used for the video.".into(),
                }
            })
        })
        .transpose()
}

#[tauri::command]
pub fn start_export(
    app: AppHandle,
    state: State<'_, AppState>,
    exports: State<'_, Exports>,
    section: SectionId,
    request: ExportRequest,
) -> Result<u64, IpcError> {
    exports.begin(&state, section, request, move |update| emit(&app, &update))
}

#[tauri::command]
pub fn cancel_export(exports: State<'_, Exports>, id: u64) -> bool {
    exports.cancel(id)
}

/// Show an exported file in Finder (Explorer, the file manager).
#[tauri::command]
pub fn reveal_export(app: AppHandle, path: PathBuf) -> Result<(), IpcError> {
    use tauri_plugin_opener::OpenerExt as _;
    app.opener().reveal_item_in_dir(&path).map_err(|e| {
        tracing::warn!(error = %e, ?path, "could not reveal the export");
        IpcError {
            message: "The video couldn't be shown in Finder.".into(),
        }
    })
}

fn emit<R: Runtime>(app: &AppHandle<R>, update: &ExportUpdate) {
    if let Err(e) = app.emit(EXPORT_EVENT, update) {
        tracing::warn!(error = %e, "could not emit {EXPORT_EVENT}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kv_core::Project;
    use kv_session::{ExportEvent, ExportResolution, Session};

    fn state() -> AppState {
        AppState::new(Session::new(PathBuf::from("/home/ana")))
    }

    #[test]
    fn without_ffmpeg_the_dialog_is_blocked_and_start_fails() {
        let exports = Exports::with_backend(Err("Export is unavailable: no FFmpeg".into()));
        let st = state();
        let o = exports.options(&st, Project::DEFAULT_SECTION).unwrap();
        assert_eq!(
            o.blocked.as_deref(),
            Some("Export is unavailable: no FFmpeg")
        );
        let req = ExportRequest {
            codec: kv_media::VideoCodec::H264,
            resolution: ExportResolution::Hd1080,
            aspect: kv_core::AspectRatio::Landscape16x9,
            path: "/home/ana/Movies/Untitled.mp4".into(),
        };
        let e = exports
            .begin(&st, Project::DEFAULT_SECTION, req, |_| {})
            .unwrap_err();
        assert_eq!(e.message, "Export is unavailable: no FFmpeg");
        assert!(!exports.cancel(1));
    }

    #[test]
    fn empty_edit_cannot_start() {
        let exports = Exports::start();
        let st = state();
        let req = ExportRequest {
            codec: kv_media::VideoCodec::H264,
            resolution: ExportResolution::Hd1080,
            aspect: kv_core::AspectRatio::Square1x1,
            path: "/tmp/x.mp4".into(),
        };
        let e = exports
            .begin(&st, Project::DEFAULT_SECTION, req, |_| {})
            .unwrap_err();
        assert!(e.message.starts_with("Nothing to export yet"), "{e:?}");
    }

    #[test]
    fn update_json_flattens_the_event() {
        let u = ExportUpdate {
            id: 3,
            event: ExportEvent::Failed {
                message: "disk full".into(),
            },
        };
        assert_eq!(
            serde_json::to_value(&u).unwrap(),
            serde_json::json!({ "id": 3, "state": "failed", "message": "disk full" })
        );
    }
}
