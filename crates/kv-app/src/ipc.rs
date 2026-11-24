//! The IPC handlers behind the Tauri commands, as plain functions over
//! [`AppState`] so they can be tested without a window or a Tauri runtime.
//!
//! Each handler locks the [`Session`], forwards the call and returns the new
//! [`Snapshot`] or an [`IpcError`]. Emitting `kv://state` is the Tauri
//! command's job (see `main.rs`), not the handler's.

use kv_core::Command;
use kv_session::{Session, SessionError, Snapshot};
use serde::Serialize;
use std::sync::{Mutex, MutexGuard};

/// The event the backend emits with the new [`Snapshot`] after every change.
pub const STATE_EVENT: &str = "kv://state";

/// Tauri managed state: the window's one session.
pub struct AppState {
    session: Mutex<Session>,
}

impl AppState {
    pub fn new(session: Session) -> AppState {
        AppState {
            session: Mutex::new(session),
        }
    }

    fn session(&self) -> Result<MutexGuard<'_, Session>, IpcError> {
        // A panic while the lock was held may have left the session half
        // changed, so refuse to keep working on it rather than recover.
        self.session.lock().map_err(|_| IpcError {
            message: "kwikvid hit an internal error. Please restart it.".into(),
        })
    }
}

/// What a failed command returns to the UI: `{ "message": "..." }`, a short
/// sentence the UI shows as a toast.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct IpcError {
    pub message: String,
}

impl From<SessionError> for IpcError {
    fn from(e: SessionError) -> IpcError {
        tracing::warn!(error = ?e, "session action rejected");
        IpcError {
            message: e.to_string(),
        }
    }
}

/// Apply a command; it becomes undoable.
pub fn apply(state: &AppState, cmd: Command) -> Result<Snapshot, IpcError> {
    Ok(state.session()?.apply(cmd)?)
}

/// Undo the last applied command.
pub fn undo(state: &AppState) -> Result<Snapshot, IpcError> {
    Ok(state.session()?.undo()?)
}

/// Redo the last undone command.
pub fn redo(state: &AppState) -> Result<Snapshot, IpcError> {
    Ok(state.session()?.redo()?)
}

/// The current state, unchanged.
pub fn get_state(state: &AppState) -> Result<Snapshot, IpcError> {
    Ok(state.session()?.snapshot())
}

#[cfg(test)]
mod tests {
    use super::*;
    use kv_core::{Project, SectionId};
    use std::path::PathBuf;
    use std::sync::Arc;

    const S: SectionId = Project::DEFAULT_SECTION;

    fn state() -> AppState {
        AppState::new(Session::new(PathBuf::from("/home/ana")))
    }

    fn set_seed(seed: u64) -> Command {
        Command::SetSeed { section: S, seed }
    }

    #[test]
    fn get_state_returns_the_initial_snapshot() {
        let s = get_state(&state()).unwrap();
        assert_eq!(s.revision, 0);
        assert!(!s.can_undo && !s.can_redo && !s.dirty);
    }

    #[test]
    fn apply_undo_redo_forward_to_the_session() {
        let st = state();
        let a = apply(&st, set_seed(7)).unwrap();
        assert_eq!(a.revision, 1);
        assert!(a.can_undo && !a.can_redo && a.dirty);
        assert_eq!(a.project.sections[0].seed, 7);

        let u = undo(&st).unwrap();
        assert_eq!(u.revision, 2);
        assert!(!u.can_undo && u.can_redo && !u.dirty);
        assert_ne!(u.project.sections[0].seed, 7);

        let r = redo(&st).unwrap();
        assert_eq!(r.revision, 3);
        assert_eq!(r.project.sections[0].seed, 7);
        assert_eq!(get_state(&st).unwrap(), r);
    }

    #[test]
    fn errors_come_back_as_a_message_and_change_nothing() {
        let st = state();
        assert_eq!(
            undo(&st).unwrap_err(),
            IpcError {
                message: "Nothing to undo.".into()
            }
        );
        assert_eq!(redo(&st).unwrap_err().message, "Nothing to redo.");

        let bad = Command::SetSeed {
            section: SectionId(99),
            seed: 1,
        };
        let e = apply(&st, bad).unwrap_err();
        assert_eq!(e.message, "That part of the project no longer exists.");
        assert_eq!(get_state(&st).unwrap().revision, 0);
    }

    #[test]
    fn ipc_error_serialises_as_message_object() {
        let e = IpcError {
            message: "Nothing to undo.".into(),
        };
        assert_eq!(
            serde_json::to_value(&e).unwrap(),
            serde_json::json!({ "message": "Nothing to undo." })
        );
    }

    #[test]
    fn commands_from_the_ui_deserialise() {
        // What `api.apply(cmd)` sends: kv-core's tagged JSON.
        let cmd: Command =
            serde_json::from_value(serde_json::json!({"type":"set_seed","section":1,"seed":42}))
                .unwrap();
        let snap = apply(&state(), cmd).unwrap();
        assert_eq!(snap.project.sections[0].seed, 42);
        let json = serde_json::to_value(&snap).unwrap();
        assert_eq!(json["can_undo"], true);
        assert_eq!(json["revision"], 1);
    }

    #[test]
    fn poisoned_session_is_refused() {
        let st = Arc::new(state());
        let st2 = Arc::clone(&st);
        let _ = std::thread::spawn(move || {
            let _guard = st2.session.lock().unwrap();
            panic!("poison the lock");
        })
        .join();
        let e = get_state(&st).unwrap_err();
        assert!(e.message.contains("internal error"), "{e:?}");
    }
}
