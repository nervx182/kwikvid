//! kwikvid desktop app: a thin Tauri 2 shell over [`kv_session::Session`].
//!
//! The UI (plain HTML/CSS/ES modules in `ui/`, no bundler) calls four
//! commands: `apply`, `undo`, `redo` and `get_state`. Each forwards to the
//! session (see [`ipc`]); after every change the backend emits a `kv://state`
//! event with the new snapshot. Errors come back as `{ "message": "..." }`.
//! Project files (New, Open, Save, Save As, recent) are in [`project`]; the
//! native menu bar, which emits `kv://menu` actions, is in [`menu`].
//!
//! `--smoke` quits with code 0 after the first `kv://state` event (sent when
//! the page has loaded), or with code 3 if none arrives in time. CI and
//! headless checks use it to prove the app starts and exits cleanly.

// Release builds on Windows are GUI apps without a console window.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod ipc;
mod menu;
mod project;

use std::path::PathBuf;
use std::time::Duration;

use anyhow::Context as _;
use ipc::{AppState, IpcError, STATE_EVENT};
use kv_core::Command;
use kv_session::{Session, Snapshot};
use project::ProjectFiles;
use tauri::webview::PageLoadEvent;
use tauri::{AppHandle, Emitter, Listener, Manager, RunEvent, Runtime, State, WindowEvent};

/// How long `--smoke` waits for the first `kv://state` event.
const SMOKE_TIMEOUT: Duration = Duration::from_secs(60);

/// Send the new state to every window after a successful change.
fn emit_state<R: Runtime>(app: &AppHandle<R>, snapshot: &Snapshot) {
    if let Err(e) = app.emit(STATE_EVENT, snapshot) {
        tracing::warn!(error = %e, "could not emit {STATE_EVENT}");
    }
}

fn changed<R: Runtime>(
    app: &AppHandle<R>,
    result: Result<Snapshot, IpcError>,
) -> Result<Snapshot, IpcError> {
    if let Ok(snapshot) = &result {
        emit_state(app, snapshot);
    }
    result
}

#[tauri::command]
fn apply(app: AppHandle, state: State<'_, AppState>, cmd: Command) -> Result<Snapshot, IpcError> {
    changed(&app, ipc::apply(&state, cmd))
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

/// Command-line options. Unknown arguments are ignored (macOS may pass
/// `-psn_...` when launched from Finder).
#[derive(Debug, Default, PartialEq, Eq)]
struct Options {
    smoke: bool,
}

impl Options {
    fn parse(args: impl IntoIterator<Item = String>) -> Options {
        let mut opts = Options::default();
        for a in args {
            if a == "--smoke" {
                opts.smoke = true;
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
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    let opts = Options::parse(std::env::args().skip(1));

    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
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
        ])
        .setup(move |app| {
            let root = media_root(app.handle());
            tracing::info!(media_root = %root.display(), "new session");
            app.manage(AppState::new(Session::new(root)));
            let files = ProjectFiles::load(app.path().app_config_dir().ok());
            app.set_menu(menu::build(app.handle(), &files.recent())?)?;
            app.manage(files);
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
