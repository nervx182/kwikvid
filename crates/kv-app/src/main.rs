//! kwikvid desktop app: a thin Tauri 2 shell over [`kv_session::Session`].
//!
//! The UI (plain HTML/CSS/ES modules in `ui/`, no bundler) calls five
//! commands: `apply`, `undo`, `redo`, `get_state` and `reroll`, plus
//! per-feature ones registered from their own module (the timeline's
//! `get_beats`, the library's `import` and `pick_media`; the transport's
//! `play`, `pause`, `seek`, `get_transport` and `get_preview_stats`, see
//! [`transport`]; the music picker's `bundled_tracks`, `pick_music` and
//! `analyze_music`, see [`music`]; the export dialog's `export_options`,
//! `choose_export_path`, `start_export`, `cancel_export` and
//! `reveal_export`, see [`export`]). Each forwards to the
//! session (see [`ipc`]); after every change the backend emits a `kv://state`
//! event with the new snapshot. Errors come back as `{ "message": "..." }`.
//! Project files (New, Open, Save, Save As, recent) are in [`project`]; the
//! native menu bar, which emits `kv://menu` actions, is in [`menu`].
//!
//! `--smoke` quits with code 0 after the first `kv://state` event (sent when
//! the page has loaded), or with code 3 if none arrives in time. CI and
//! headless checks use it to prove the app starts and exits cleanly.
//!
//! Preview (ADR 0007): the UI reports the viewer rectangle with
//! `set_viewer_rect` and the backend chosen in [`preview::PreviewBackend`]
//! draws there; on macOS that is the native Metal layer (`native_view`),
//! elsewhere (or with `KV_PREVIEW=webgl`) raw frames over `kvframe://`
//! into a WebGL2 canvas ([`webgl`]); the UI asks which with
//! `get_preview_backend`. `--preview-test-pattern` shows a moving test
//! pattern through it. The preview engine (kv-render) plays the edit
//! there, driven by the transport ([`transport`]); without a preview
//! surface (`KV_PREVIEW=off`) it still runs, showing nothing, so the
//! transport works.

// Release builds on Windows are GUI apps without a console window.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod blit;
mod export;
mod ipc;
mod library;
mod menu;
mod music;
#[cfg(target_os = "macos")]
mod native_view;
mod preview;
mod project;
mod timeline;
mod transport;
mod webgl;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context as _;
use ipc::{AppState, IpcError, STATE_EVENT};
use kv_core::{Command, SectionId};
use kv_media::MediaBackend;
use kv_render::preview::{AudioTransport, CpuCompositor};
use kv_session::regen::RegenSnapshot;
use kv_session::{Session, Snapshot};
use preview::{PreviewBackend, PreviewState, ViewerRect};
use project::ProjectFiles;
use serde::Serialize;
use tauri::webview::PageLoadEvent;
use tauri::{
    AppHandle, Emitter, Listener, Manager, RunEvent, Runtime, State, WebviewWindow, WindowEvent,
};
use tracing_subscriber::prelude::*;
use transport::{SeekLatencies, Transport};

/// How long `--smoke` waits for the first `kv://state` event.
const SMOKE_TIMEOUT: Duration = Duration::from_secs(60);

/// Send the new state to every window after a successful change.
/// `snapshot` is a [`Snapshot`] or a [`RegenSnapshot`] (the same keys plus
/// `manual_edits_replaced` and `regen_error`).
pub(crate) fn emit_state<R: Runtime, S: Serialize + Clone>(app: &AppHandle<R>, snapshot: &S) {
    if let Err(e) = app.emit(STATE_EVENT, snapshot) {
        tracing::warn!(error = %e, "could not emit {STATE_EVENT}");
    }
    // The preview follows every change (edits keep playing).
    transport::project_changed(app);
}

/// What every preview engine is built from: the decoder (none when FFmpeg
/// failed to start) and the audio output (none without a device).
struct EngineParts {
    media: Option<Arc<dyn MediaBackend>>,
    audio: Option<Arc<AudioTransport>>,
}

impl EngineParts {
    fn start() -> EngineParts {
        let media = match kv_media::FfmpegBackend::new() {
            Ok(b) => Some(Arc::new(b) as Arc<dyn MediaBackend>),
            Err(e) => {
                tracing::warn!("no media backend, preview off: {e}");
                None
            }
        };
        // The output stream must stay alive (and on this thread, it is not
        // `Send`) for the app's lifetime, like the native view.
        let audio = kv_render::preview::audio::start_default_output().map(|(out, t)| {
            std::mem::forget(out);
            t
        });
        EngineParts { media, audio }
    }

    /// Engines drawing nowhere: the transport and clock work, nothing is
    /// shown. Composites at a quarter of the preview size to stay cheap.
    fn headless(&self) -> Option<transport::EngineFactory> {
        let media = self.media.clone()?;
        Some(transport::engine_factory(
            media,
            self.audio.clone(),
            |(w, h)| {
                let c = CpuCompositor::new((w / 4).max(2), (h / 4).max(2))
                    .map_err(|e| e.to_string())?;
                Ok((Box::new(c) as _, Box::new(transport::NullSink) as _))
            },
        ))
    }
}

impl EngineParts {
    /// Engines for the WebGL backend: the CPU compositor at the preview
    /// size capped to the 960x540 proxy, drawing into `sink`.
    fn webgl(&self, sink: webgl::WebGlSink) -> Option<transport::EngineFactory> {
        let media = self.media.clone()?;
        Some(transport::engine_factory(
            media,
            self.audio.clone(),
            move |size| {
                let (w, h) = webgl::capped_size(size);
                let c = CpuCompositor::new(w, h).map_err(|e| e.to_string())?;
                Ok((Box::new(c) as _, Box::new(sink.clone()) as _))
            },
        ))
    }
}

fn changed<R: Runtime, S: Serialize + Clone>(
    app: &AppHandle<R>,
    result: Result<S, IpcError>,
) -> Result<S, IpcError> {
    if let Ok(snapshot) = &result {
        emit_state(app, snapshot);
    }
    result
}

#[tauri::command]
fn apply(
    app: AppHandle,
    state: State<'_, AppState>,
    cmd: Command,
) -> Result<RegenSnapshot, IpcError> {
    changed(&app, ipc::apply(&state, cmd))
}

#[tauri::command]
fn reroll(
    app: AppHandle,
    state: State<'_, AppState>,
    section: SectionId,
) -> Result<RegenSnapshot, IpcError> {
    changed(&app, ipc::reroll(&state, section))
}

#[tauri::command]
fn undo(app: AppHandle, state: State<'_, AppState>) -> Result<Snapshot, IpcError> {
    changed(&app, ipc::undo(&state))
}

#[tauri::command]
fn redo(app: AppHandle, state: State<'_, AppState>) -> Result<Snapshot, IpcError> {
    changed(&app, ipc::redo(&state))
}

#[tauri::command]
fn get_state(state: State<'_, AppState>) -> Result<Snapshot, IpcError> {
    ipc::get_state(&state)
}

/// The viewer rectangle (CSS pixels relative to the webview, and the device
/// pixel ratio), sent by `ui/js/viewer.js` on load, resize, layout change
/// and fullscreen. Runs off the main thread: the native layer waits for it.
#[tauri::command(async)]
fn set_viewer_rect(
    window: WebviewWindow,
    preview: State<'_, PreviewState>,
    x: f64,
    y: f64,
    w: f64,
    h: f64,
    scale: f64,
) -> Result<(), IpcError> {
    let rect = ViewerRect { x, y, w, h, scale }
        .validate()
        .map_err(|message| IpcError { message })?;
    window
        .state::<Arc<Transport>>()
        .set_viewer(rect.physical_size());
    // Only the native backend needs these.
    let _ = (&window, rect);
    match preview.backend() {
        #[cfg(target_os = "macos")]
        PreviewBackend::NativeMetal => {
            let pattern = if preview.test_pattern() {
                Some(
                    ipc::get_state(&window.state::<AppState>())?
                        .project
                        .settings
                        .aspect_ratio,
                )
            } else {
                None
            };
            let native = window.state::<native_view::NativePreview>();
            match native.view(&window, pattern) {
                Ok(view) => {
                    let transport = window.state::<Arc<Transport>>();
                    let parts = window.state::<EngineParts>();
                    if pattern.is_none() && !transport.has_factory() {
                        if let Some(media) = parts.media.clone() {
                            let (gpu, sink) = (view.gpu(), view.sink());
                            transport.set_factory(transport::engine_factory(
                                media,
                                parts.audio.clone(),
                                move |(w, h)| {
                                    let c = kv_render::gpu::GpuCompositor::new(&*gpu, w, h)
                                        .map_err(|e| e.to_string())?;
                                    Ok((Box::new(c) as _, Box::new(sink.clone()) as _))
                                },
                            ));
                        }
                    }
                    if let Err(e) = view.set_rect(rect) {
                        tracing::warn!(error = %e, "could not resize the native preview layer");
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, "native preview layer unavailable; viewer off");
                    preview.set_backend(PreviewBackend::Off);
                }
            }
        }
        PreviewBackend::WebGl => {
            if preview.test_pattern() {
                let aspect = ipc::get_state(&window.state::<AppState>())?
                    .project
                    .settings
                    .aspect_ratio;
                window
                    .state::<webgl::WebGlPreview>()
                    .start_test_pattern_once(aspect);
            }
        }
        PreviewBackend::Off => {}
    }
    Ok(())
}

/// The preview backend in use, by [`PreviewBackend::name`]: `"webgl"`
/// makes `ui/js/viewer-webgl.js` draw the frames itself.
#[tauri::command]
fn get_preview_backend(preview: State<'_, PreviewState>) -> &'static str {
    preview.backend().name()
}

/// Command-line options. Unknown arguments are ignored (macOS may pass
/// `-psn_...` when launched from Finder).
#[derive(Debug, Default, PartialEq, Eq)]
struct Options {
    smoke: bool,
    /// `--preview-test-pattern`: show a moving pattern in the viewer through
    /// the preview backend, for manual checks.
    preview_test_pattern: bool,
}

impl Options {
    fn parse(args: impl IntoIterator<Item = String>) -> Options {
        let mut opts = Options::default();
        for a in args {
            match a.as_str() {
                "--smoke" => opts.smoke = true,
                "--preview-test-pattern" => opts.preview_test_pattern = true,
                _ => {}
            }
        }
        opts
    }
}

/// An untitled project's media paths are relative to the home directory
/// (see `kv-session`).
fn media_root<R: Runtime>(app: &AppHandle<R>) -> PathBuf {
    app.path()
        .home_dir()
        .or_else(|_| std::env::current_dir())
        .unwrap_or_else(|_| PathBuf::from("/"))
}

fn main() -> anyhow::Result<()> {
    let latencies = Arc::new(SeekLatencies::default());
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::fmt::layer()
                .with_writer(std::io::stderr)
                .with_filter(
                    tracing_subscriber::EnvFilter::try_from_default_env()
                        .unwrap_or_else(|_| "info".into()),
                ),
        )
        .with(transport::seek_latency_layer(Arc::clone(&latencies)))
        .init();
    let opts = Options::parse(std::env::args().skip(1));

    let latest_frame = Arc::new(webgl::LatestFrame::default());
    let builder = webgl::register(tauri::Builder::default(), Arc::clone(&latest_frame));
    library::register(builder)
        .plugin(tauri_plugin_opener::init())
        .invoke_handler(tauri::generate_handler![
            apply,
            undo,
            redo,
            get_state,
            project::project_info,
            project::new_project,
            project::open_project,
            project::save_project,
            project::save_project_as,
            project::choose_open_path,
            project::choose_save_path,
            project::set_window_title,
            project::quit,
            reroll,
            set_viewer_rect,
            get_preview_backend,
            timeline::get_beats,
            timeline::nudge_cut,
            library::import,
            library::pick_media,
            transport::play,
            transport::pause,
            transport::seek,
            transport::get_transport,
            transport::get_preview_stats,
            music::bundled_tracks,
            music::pick_music,
            music::analyze_music,
            export::export_options,
            export::choose_export_path,
            export::start_export,
            export::cancel_export,
            export::reveal_export
        ])
        .setup(move |app| {
            let root = media_root(app.handle());
            tracing::info!(media_root = %root.display(), "new session");
            app.manage(AppState::new(Session::new(root)));
            // Before the library's job event thread starts, which reads it.
            app.manage(music::MusicPicks::default());
            app.manage(library::Library::start(app.handle()));
            app.manage(export::Exports::start());
            let files = ProjectFiles::load(app.path().app_config_dir().ok());
            app.set_menu(menu::build(app.handle(), &files.recent())?)?;
            app.manage(files);
            let backend = PreviewBackend::preferred();
            tracing::info!(backend = backend.name(), "preview backend");
            if opts.preview_test_pattern && backend == PreviewBackend::Off {
                tracing::warn!("--preview-test-pattern: preview backend is off");
            }
            app.manage(PreviewState::new(backend, opts.preview_test_pattern));
            let webgl = webgl::WebGlPreview::new(latest_frame);
            let transport = Arc::new(Transport::new(latencies));
            let parts = EngineParts::start();
            match backend {
                PreviewBackend::Off => {
                    if let Some(f) = parts.headless() {
                        transport.set_factory(f);
                    }
                }
                // The test pattern, when asked for, has the canvas alone.
                PreviewBackend::WebGl if !opts.preview_test_pattern => {
                    if let Some(f) = parts.webgl(webgl.sink()) {
                        transport.set_factory(f);
                    }
                }
                _ => {}
            }
            app.manage(webgl);
            app.manage(parts);
            transport.spawn_worker();
            transport.spawn_position_events(app.handle().clone());
            app.manage(transport);
            #[cfg(target_os = "macos")]
            app.manage(native_view::NativePreview::default());
            if opts.smoke {
                let handle = app.handle().clone();
                app.listen_any(STATE_EVENT, move |_| {
                    tracing::info!("smoke: first {STATE_EVENT} received, exiting");
                    handle.exit(0);
                });
                std::thread::spawn(|| {
                    std::thread::sleep(SMOKE_TIMEOUT);
                    eprintln!("smoke: no {STATE_EVENT} within {SMOKE_TIMEOUT:?}");
                    std::process::exit(3);
                });
            }
            Ok(())
        })
        // A freshly loaded (or reloaded) page gets the current state pushed,
        // in addition to asking for it with `get_state`.
        .on_page_load(|webview, payload| {
            if payload.event() == PageLoadEvent::Finished {
                let app = webview.app_handle();
                match ipc::get_state(&app.state::<AppState>()) {
                    Ok(snapshot) => emit_state(app, &snapshot),
                    Err(e) => tracing::warn!(?e, "no state to send on page load"),
                }
            }
        })
        .on_menu_event(|app, event| {
            let recent = app.state::<ProjectFiles>().recent();
            if let Some(action) = menu::action_for(event.id().as_ref(), &recent) {
                menu::emit_action(app, &action);
            }
        })
        // Closing the window with unsaved changes asks the UI to run the
        // Save / Don't Save / Cancel prompt; it calls `quit` when done.
        .on_window_event(|window, event| {
            if let WindowEvent::CloseRequested { api, .. } = event {
                let app = window.app_handle();
                if quit_needs_prompt(app) {
                    api.prevent_close();
                    menu::emit_action(app, &menu::MenuAction::Quit);
                }
            }
        })
        .build(tauri::generate_context!())
        .context("building the kwikvid app")?
        .run(|app, event| {
            // A user-initiated exit (no exit code), e.g. Quit from the Dock.
            if let RunEvent::ExitRequested {
                code: None, api, ..
            } = event
            {
                if quit_needs_prompt(app) {
                    api.prevent_exit();
                    menu::emit_action(app, &menu::MenuAction::Quit);
                }
            }
        });
    Ok(())
}

fn quit_needs_prompt<R: Runtime>(app: &AppHandle<R>) -> bool {
    match (app.try_state::<ProjectFiles>(), app.try_state::<AppState>()) {
        (Some(files), Some(state)) => files.needs_quit_prompt(&state),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_smoke_flag_and_ignores_unknown_args() {
        let args = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert!(!Options::parse(args(&[])).smoke);
        assert!(Options::parse(args(&["-psn_0_123", "--smoke"])).smoke);
        let o = Options::parse(args(&["--preview-test-pattern"]));
        assert!(o.preview_test_pattern && !o.smoke);
        assert!(!Options::parse(args(&["--smoke"])).preview_test_pattern);
    }

    /// The window settings the native preview layer depends on (ADR 0007).
    #[test]
    fn config_has_transparent_window_and_private_api() {
        let conf: serde_json::Value =
            serde_json::from_str(include_str!("../tauri.conf.json")).unwrap();
        assert_eq!(conf["app"]["macOSPrivateApi"], true);
        assert_eq!(conf["app"]["withGlobalTauri"], true);
        assert_eq!(conf["app"]["windows"][0]["transparent"], true);
        assert_eq!(conf["build"]["frontendDist"], "ui");
    }

    /// The page may fetch WebGL preview frames (`kvframe`, src/webgl.rs).
    #[test]
    fn csp_allows_frame_protocol() {
        let conf: serde_json::Value =
            serde_json::from_str(include_str!("../tauri.conf.json")).unwrap();
        let csp = conf["app"]["security"]["csp"].as_str().unwrap();
        let connect = csp
            .split(';')
            .map(str::trim)
            .find(|d| d.starts_with("connect-src"))
            .expect("connect-src directive");
        for src in [
            "ipc:",
            "http://ipc.localhost",
            "kvframe:",
            "http://kvframe.localhost",
        ] {
            assert!(connect.split(' ').any(|s| s == src), "{src} in {connect}");
        }
    }

    /// `ui/js/api.js` is the only module that touches `window.__TAURI__`, so
    /// UI tests can swap it for a mock.
    #[test]
    fn only_api_js_touches_tauri_global() {
        fn walk(dir: &std::path::Path, out: &mut Vec<PathBuf>) {
            for entry in std::fs::read_dir(dir).unwrap() {
                let p = entry.unwrap().path();
                if p.is_dir() {
                    walk(&p, out);
                } else {
                    out.push(p);
                }
            }
        }
        let ui = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("ui");
        let mut files = Vec::new();
        walk(&ui, &mut files);
        let users: Vec<_> = files
            .iter()
            .filter(|p| {
                std::fs::read_to_string(p)
                    .map(|s| s.contains("__TAURI__"))
                    .unwrap_or(false)
            })
            .map(|p| {
                p.strip_prefix(&ui)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/")
            })
            .collect();
        assert_eq!(users, vec!["js/api.js".to_string()]);
    }
}
