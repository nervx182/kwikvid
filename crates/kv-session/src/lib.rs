//! kwikvid desktop session: the app's logic without Tauri.
//!
//! A [`Session`] owns one project (wrapped in a [`kv_core::History`] for
//! undo/redo) and its media root, applies [`kv_core::Command`]s, and returns a
//! [`Snapshot`] of the state for the UI to render. `kv-app` is a thin IPC
//! layer over this crate, so everything here is unit-testable without a
//! webview. One `Session` per window/project.
//!
//! The crate has no platform code. File I/O happens in project save/open
//! and the recent projects list ([`Session::save_as`], [`Session::save`],
//! [`Session::open`], [`RecentProjects`]), and in background jobs
//! ([`JobRunner`]) through injected backends and `kv-cache`.
//! Regenerate/re-roll lives in [`regen`], import and library thumbnails in
//! [`import`], export in [`export`].
//!
//! # Paths
//!
//! Every `MediaItem.path` / `MusicRef.path` in a project is relative to the
//! session's media root and `/`-separated. [`Session::to_project_path`] and
//! [`Session::resolve`] convert between host paths and project paths with
//! pure lexical arithmetic (no `canonicalize`), using `..` for files outside
//! the root. An untitled project's media root is the user's home directory;
//! [`Session::save_as`] rebases the paths onto the project file's folder,
//! and [`Session::open`] uses the file's folder as the media root.
//!
//! # Snapshot JSON
//!
//! [`Snapshot`] serialises with **snake_case** keys, matching `kv-core`'s
//! commands and project file:
//!
//! ```json
//! {"revision":3,"project":{"version":1,...},"can_undo":true,
//!  "can_redo":false,"dirty":true,"media_root":"/Users/ana"}
//! ```

mod analysis;
mod error;
pub mod export;
mod filmstrips;
pub mod import;
mod jobs;
mod paths;
mod project_file;
pub mod regen;

pub use analysis::{AudioAnalyzer, MusicAnalyzer};
pub use error::SessionError;
pub use export::{
    export_size, ExportEvent, ExportInput, ExportJob, ExportOptions, ExportRequest,
    ExportResolution, ExportSummary, EXPORT_FPS,
};
pub use filmstrips::{
    FilmstripGenerator, MediaFilmstrips, FILMSTRIP_COUNT, FILMSTRIP_HEIGHT, FILMSTRIP_JPEG_QUALITY,
};
pub use import::{ImportReport, IMPORT_EXTENSIONS};
pub use jobs::*;
pub use project_file::{
    rebase, MissingFile, MissingKind, ProjectFileError, RecentProjects, PROJECT_EXTENSION,
};

use kv_core::{Command, History, Project, ProjectId};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Everything the UI needs to render the session, as plain data.
///
/// Keys are snake_case in JSON (see the crate docs).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Snapshot {
    /// Bumped by every change (apply, undo, redo); unchanged by a rejected
    /// action. Lets the UI drop stale snapshots.
    pub revision: u64,
    /// The full current project.
    pub project: Project,
    pub can_undo: bool,
    pub can_redo: bool,
    /// The project differs from its state at the last [`Session::mark_saved`]
    /// (or at creation). Undoing back to the saved state clears it.
    pub dirty: bool,
    /// The directory every project media path is relative to.
    pub media_root: PathBuf,
}

/// One open project: undo history, media root and change tracking.
#[derive(Debug, Clone)]
pub struct Session {
    history: History,
    media_root: PathBuf,
    revision: u64,
    /// The project as last saved (or as created).
    saved: Project,
    /// Finished music analyses (see [`Session::music_analysis`]).
    music: analysis::MusicAnalyses,
    /// Finished filmstrips (see [`Session::thumbnail`]).
    thumbnails: import::Thumbnails,
    /// The project file, once opened or saved.
    file: Option<PathBuf>,
}

impl Session {
    /// A session with a new, empty project whose media paths are relative to
    /// `media_root` (an absolute directory).
    pub fn new(media_root: PathBuf) -> Session {
        let project = Project::new(ProjectId(1));
        Session {
            saved: project.clone(),
            history: History::new(project),
            media_root,
            revision: 0,
            music: Default::default(),
            thumbnails: Default::default(),
            file: None,
        }
    }

    /// The current project.
    pub fn project(&self) -> &Project {
        self.history.project()
    }

    /// The directory every project media path is relative to.
    pub fn media_root(&self) -> &Path {
        &self.media_root
    }

    /// The current revision (see [`Snapshot::revision`]).
    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// Whether the project changed since the last [`Session::mark_saved`].
    pub fn is_dirty(&self) -> bool {
        *self.history.project() != self.saved
    }

    /// Apply a command. On success it is undoable and the revision is
    /// bumped; on error neither the project nor the revision changes.
    pub fn apply(&mut self, cmd: Command) -> Result<Snapshot, SessionError> {
        match self.history.apply(cmd) {
            Ok(event) => {
                self.revision += 1;
                tracing::debug!(revision = self.revision, ?event, "command applied");
                Ok(self.snapshot())
            }
            Err(e) => {
                tracing::debug!(revision = self.revision, error = %e, "command rejected");
                Err(e.into())
            }
        }
    }

    /// Undo the last applied command.
    pub fn undo(&mut self) -> Result<Snapshot, SessionError> {
        self.history.undo().ok_or(SessionError::NothingToUndo)?;
        self.revision += 1;
        tracing::debug!(revision = self.revision, "undo");
        Ok(self.snapshot())
    }

    /// Redo the last undone command.
    pub fn redo(&mut self) -> Result<Snapshot, SessionError> {
        self.history.redo().ok_or(SessionError::NothingToRedo)?;
        self.revision += 1;
        tracing::debug!(revision = self.revision, "redo");
        Ok(self.snapshot())
    }

    /// Continue the revision numbering of a session this one replaces (New
    /// or Open in the same window): the revision becomes `previous + 1`, so
    /// the UI's stale-snapshot check never drops the replacement's state.
    pub fn continue_revisions(&mut self, previous: u64) {
        self.revision = previous.saturating_add(1);
    }

    /// Record the current project as saved, clearing [`Snapshot::dirty`].
    /// Does not write anything (see [`Session::save`]).
    pub fn mark_saved(&mut self) {
        self.saved = self.history.project().clone();
    }

    /// The current state for the UI.
    pub fn snapshot(&self) -> Snapshot {
        Snapshot {
            revision: self.revision,
            project: self.history.project().clone(),
            can_undo: self.history.can_undo(),
            can_redo: self.history.can_redo(),
            dirty: self.is_dirty(),
            media_root: self.media_root.clone(),
        }
    }

    /// An absolute file path as a project path: `/`-separated, relative to
    /// the media root, with `..` steps when the file is outside it. Pure
    /// lexical arithmetic; nothing is read from disk.
    ///
    /// Errors: the path is relative, the root is relative, the path is on
    /// another drive or share, the path is the root itself, or it is not
    /// UTF-8.
    pub fn to_project_path(&self, path: &Path) -> Result<String, SessionError> {
        paths::to_project_path_native(&self.media_root, path)
    }

    /// The absolute host path a project path refers to (the inverse of
    /// [`Session::to_project_path`]).
    pub fn resolve(&self, project_path: &str) -> PathBuf {
        paths::resolve_native(&self.media_root, project_path)
    }
}
