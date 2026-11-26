//! The IPC handlers behind the Tauri commands, as plain functions over
//! [`AppState`] so they can be tested without a window or a Tauri runtime.
//!
//! Each handler locks the [`Session`], forwards the call and returns the new
//! [`Snapshot`] (or [`RegenSnapshot`], for actions that can rebuild the
//! auto-edit) or an [`IpcError`]. Emitting `kv://state` is the Tauri
//! command's job (see `main.rs`), not the handler's.

use kv_core::{Command, SectionId};
use kv_session::regen::{RegenError, RegenSnapshot};
use kv_session::{Session, SessionError, Snapshot};
use serde::Serialize;
use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard};

/// The event the backend emits with the new [`Snapshot`] after every change.
pub const STATE_EVENT: &str = "kv://state";

/// Tauri managed state: the window's one session.
pub struct AppState {
    session: Mutex<Session>,
    /// The app's bundled music folder, given to every session (see
    /// [`Session::set_bundled_music_dir`]).
    bundled_music: Option<PathBuf>,
}

impl AppState {
    pub fn new(session: Session) -> AppState {
        AppState {
            session: Mutex::new(session),
            bundled_music: None,
        }
    }

    /// Give every session, including ones that later replace it (New,
    /// Open), the bundled music folder `dir`, so saving copies bundled
    /// tracks into the project folder.
    pub fn with_bundled_music(mut self, dir: Option<PathBuf>) -> AppState {
        self.bundled_music = dir;
        self
    }

    pub(crate) fn session(&self) -> Result<MutexGuard<'_, Session>, IpcError> {
        // A panic while the lock was held may have left the session half
        // changed, so refuse to keep working on it rather than recover.
        let mut session = self.session.lock().map_err(|_| IpcError {
            message: "kwikvid hit an internal error. Please restart it.".into(),
        })?;
        // A session replaced since the last lock starts without it.
        if session.bundled_music_dir() != self.bundled_music.as_deref() {
            session.set_bundled_music_dir(self.bundled_music.clone());
        }
        Ok(session)
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

impl From<RegenError> for IpcError {
    fn from(e: RegenError) -> IpcError {
        tracing::debug!(error = ?e, "auto-edit not rebuilt");
        IpcError {
            message: e.to_string(),
        }
    }
}

/// Apply a command; it becomes undoable. Commands that change what the
/// auto-edit is built from (music, target length, pins, excludes, style,
/// media) also rebuild the edit in the same undo step (see
/// [`Session::apply_and_regenerate`]); if that rebuild is not possible the
/// command still applies and `regen_error` says why.
pub fn apply(state: &AppState, cmd: Command) -> Result<RegenSnapshot, IpcError> {
    let mut session = state.session()?;
    let analyses = session.analyses();
    Ok(session.apply_and_regenerate(cmd, &analyses)?)
}

/// Re-roll a section's auto-edit: next seed, pins kept, one undo step. On
/// error (no clips, no music, analysis pending, pins too long, ...) nothing
/// changes and the message says why.
pub fn reroll(state: &AppState, section: SectionId) -> Result<RegenSnapshot, IpcError> {
    let mut session = state.session()?;
    let analyses = session.analyses();
    Ok(session.reroll(section, &analyses)?)
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
    use kv_audio::MusicAnalysis;
    use kv_core::{MediaId, MediaItem, MediaKind, Micros, MusicRef, Project, TargetLength};
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
        let a = apply(&st, set_seed(7)).unwrap().snapshot;
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
        let snap = apply(&state(), cmd).unwrap().snapshot;
        assert_eq!(snap.project.sections[0].seed, 42);
        let json = serde_json::to_value(&snap).unwrap();
        assert_eq!(json["can_undo"], true);
        assert_eq!(json["revision"], 1);
    }

    const SEC: i64 = 1_000_000;

    /// A session with two 30 s clips and a 40 s song whose analysis is
    /// cached (beats every 0.5 s).
    fn ready_state() -> AppState {
        let mut s = Session::new(PathBuf::from("/home/ana"));
        for id in [1, 2] {
            s.apply(Command::AddMedia {
                item: MediaItem {
                    id: MediaId(id),
                    path: format!("c{id}.mp4"),
                    duration: Micros(30 * SEC),
                    kind: MediaKind::Video,
                    hilights: vec![Micros(SEC)],
                    clip_audio: false,
                    chapters: Vec::new(),
                },
            })
            .unwrap();
        }
        s.apply(Command::SetMusic {
            section: S,
            music: Some(MusicRef {
                path: "song.mp3".into(),
                duration: Micros(40 * SEC),
            }),
        })
        .unwrap();
        let beats: Vec<Micros> = (0..80).map(|i| Micros(i * SEC / 2)).collect();
        s.set_music_analysis(
            "song.mp3".into(),
            MusicAnalysis {
                duration: Micros(40 * SEC),
                tempo_bpm: 120.0,
                downbeats: beats.iter().step_by(4).copied().collect(),
                beats,
                onsets: Vec::new(),
                energy: vec![0.3; 400],
            },
        );
        AppState::new(s)
    }

    fn fixed(secs: i64) -> Command {
        Command::SetTargetLength {
            section: S,
            target: TargetLength::Fixed {
                length: Micros(secs * SEC),
            },
        }
    }

    fn edit_length(s: &Snapshot) -> i64 {
        s.project.sections[0]
            .edit
            .segments
            .iter()
            .map(|seg| seg.timeline_len().0)
            .sum()
    }

    #[test]
    fn setting_the_target_length_rebuilds_the_edit_in_one_undo_step() {
        let st = ready_state();
        let r = apply(&st, fixed(15)).unwrap();
        assert_eq!(r.regen_error, None);
        assert!(!r.manual_edits_replaced);
        assert_eq!(edit_length(&r.snapshot), 15 * SEC);
        let r = apply(&st, fixed(30)).unwrap();
        assert_eq!(edit_length(&r.snapshot), 30 * SEC);
        // Undo restores the 15 s target and its edit together.
        let u = undo(&st).unwrap();
        assert_eq!(edit_length(&u), 15 * SEC);
        assert_eq!(
            u.project.sections[0].target_length,
            TargetLength::Fixed {
                length: Micros(15 * SEC)
            }
        );
    }

    #[test]
    fn target_length_without_music_applies_and_reports_why() {
        let st = state();
        let r = apply(&st, fixed(15)).unwrap();
        assert_eq!(r.snapshot.revision, 1);
        assert_eq!(
            r.regen_error.as_deref(),
            Some("Add clips to create an edit.")
        );
        // The flattened JSON the UI reads.
        let json = serde_json::to_value(&r).unwrap();
        assert_eq!(json["regen_error"], "Add clips to create an edit.");
        assert_eq!(json["manual_edits_replaced"], false);
        assert_eq!(json["revision"], 1);
    }

    #[test]
    fn reroll_bumps_the_seed_and_changes_the_edit() {
        let st = ready_state();
        let before = apply(&st, fixed(15)).unwrap().snapshot;
        let r = reroll(&st, S).unwrap();
        let (b, a) = (&before.project.sections[0], &r.snapshot.project.sections[0]);
        assert!(a.seed > b.seed, "seed {} -> {}", b.seed, a.seed);
        assert_ne!(a.edit, b.edit);
        assert_eq!(edit_length(&r.snapshot), 15 * SEC);
        assert!(r.snapshot.can_undo);
        assert_eq!(undo(&st).unwrap().project.sections[0].edit, b.edit);
    }

    #[test]
    fn reroll_errors_change_nothing() {
        let st = state();
        assert_eq!(
            reroll(&st, S).unwrap_err().message,
            "Add clips to create an edit."
        );
        assert_eq!(
            reroll(&st, SectionId(99)).unwrap_err().message,
            "That part of the project no longer exists."
        );
        assert_eq!(get_state(&st).unwrap().revision, 0);
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
