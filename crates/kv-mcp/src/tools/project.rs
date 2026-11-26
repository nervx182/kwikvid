//! Project tools: `project_state`, `project_new`, `project_save`,
//! `project_save_as`, `project_open`.
//!
//! They mirror the desktop app's project flows (`crates/kv-app/src/project.rs`)
//! over [`kv_session::project_file`]. A project file's media paths are
//! relative to the folder holding it, which becomes the session's media root
//! ([`Session::save_as`], [`Session::open`]). The server's sandbox stays
//! `--root`, so project files must sit directly in `--root`: one in a
//! subfolder (or anywhere else) is refused. New and open replace the session
//! and discard unsaved changes; the revision numbering continues.
//!
//! After `project_open`, the music of every section is analysed (from the
//! `kv-cache` cache when unchanged; [`Session::reopen_jobs`]) before the call
//! returns, so `edit_regenerate` works at once.

use std::path::{Path, PathBuf};

use kv_session::{Job, JobEvent, JobMessage, JobRunner, MissingKind, Session, PROJECT_EXTENSION};
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ContentBlock};
use rmcp::{schemars, tool, tool_router, ErrorData};
use serde::{Deserialize, Serialize};

use super::{json_result, relative_path, snapshot_json, tool_error, WriteResult};
use crate::KwikvidServer;

/// Input of `project_save_as` and `project_open`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ProjectPathParams {
    /// The project file, directly in the project root (the server's
    /// `--root`): a file name such as `trip.kwikvid`, or an absolute path
    /// in that folder. `project_save_as` appends `.kwikvid` when the name
    /// has no extension.
    pub path: String,
}

/// A media or music file of an opened project that does not exist.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MissingOut {
    /// `media` (with `media_id`) or `music` (with `section`).
    #[serde(flatten)]
    pub kind: MissingKind,
    /// The path as stored in the project (project-relative).
    pub path: String,
}

/// A music analysis that failed after `project_open`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AnalysisError {
    /// The song, project-relative.
    pub path: String,
    pub message: String,
}

/// What the project tools (except `project_state`) return.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProjectResult {
    /// The project file, project-relative (a file name); `null` while
    /// untitled.
    pub file: Option<String>,
    /// Media and music files of the opened project that do not exist
    /// (empty for every tool but `project_open`).
    pub missing: Vec<MissingOut>,
    /// Songs that could not be analysed after `project_open`; the edit
    /// of their sections cannot be rebuilt until `music_set` succeeds.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub analysis_errors: Vec<AnalysisError>,
    #[serde(flatten)]
    pub write: WriteResult,
}

impl ProjectResult {
    fn of(session: &Session) -> Self {
        ProjectResult {
            file: session
                .file_path()
                .and_then(Path::file_name)
                .map(|n| n.to_string_lossy().into_owned()),
            missing: Vec::new(),
            analysis_errors: Vec::new(),
            write: WriteResult::from_snapshot(&session.snapshot()),
        }
    }
}

impl KwikvidServer {
    /// `path` as a project file directly in the root, or why not.
    fn project_file(&self, path: &str) -> Result<PathBuf, String> {
        let root = self.root();
        let rel = relative_path(&root, path)?;
        if rel.components().count() != 1 {
            return Err(format!(
                "{path:?}: project files must be directly in the project root {root:?} \
                 (not in a subfolder), because their media paths are relative to their folder"
            ));
        }
        Ok(root.join(rel))
    }

    pub(crate) fn project_new_impl(&self) -> Result<CallToolResult, ErrorData> {
        let mut session = self.lock();
        let mut fresh = Session::new(self.root.clone());
        fresh.continue_revisions(session.revision());
        *session = fresh;
        tracing::info!("new project");
        json_result(&ProjectResult::of(&session))
    }

    pub(crate) fn project_save_impl(&self) -> Result<CallToolResult, ErrorData> {
        let mut session = self.lock();
        match session.save() {
            Ok(_) => json_result(&ProjectResult::of(&session)),
            Err(kv_session::ProjectFileError::Untitled) => Ok(tool_error(
                "The project has no file yet; call project_save_as with a file name first.",
            )),
            Err(e) => Ok(tool_error(e.to_string())),
        }
    }

    pub(crate) fn project_save_as_impl(&self, path: &str) -> Result<CallToolResult, ErrorData> {
        let file = match self.project_file(path) {
            Ok(f) => f,
            Err(msg) => return Ok(tool_error(msg)),
        };
        if let Some(ext) = file.extension().and_then(|e| e.to_str()) {
            if !ext.eq_ignore_ascii_case(PROJECT_EXTENSION) {
                return Ok(tool_error(format!(
                    "{path:?}: project files end in .{PROJECT_EXTENSION}"
                )));
            }
        }
        let mut session = self.lock();
        match session.save_as(&file) {
            Ok(_) => json_result(&ProjectResult::of(&session)),
            Err(e) => Ok(tool_error(e.to_string())),
        }
    }

    pub(crate) fn project_open_impl(&self, path: &str) -> Result<CallToolResult, ErrorData> {
        let file = match self.project_file(path) {
            Ok(f) => f,
            Err(msg) => return Ok(tool_error(msg)),
        };
        let (mut opened, missing) = match Session::open(&file) {
            Ok(r) => r,
            Err(e) => {
                let detail = match &e {
                    kv_session::ProjectFileError::Io { source, .. } => format!(" ({source})"),
                    _ => String::new(),
                };
                return Ok(tool_error(format!("{e}{detail}")));
            }
        };

        // Analyse the music (cache first) before the session is swapped in,
        // without holding the session lock.
        let jobs: Vec<Job> = opened
            .reopen_jobs()
            .into_iter()
            .filter(|j| matches!(j, Job::AnalyzeMusic(_)))
            .collect();
        let mut analysis_errors = Vec::new();
        if !jobs.is_empty() {
            let (runner, events) = JobRunner::with_workers(self.ctx.clone(), 1);
            let mut pending: Vec<_> = jobs.into_iter().map(|j| runner.submit(j)).collect();
            let mut finished: Vec<JobMessage> = Vec::new();
            while !pending.is_empty() {
                match events.recv() {
                    Ok(msg) if msg.event.is_final() && pending.contains(&msg.id) => {
                        pending.retain(|id| *id != msg.id);
                        finished.push(msg);
                    }
                    Ok(_) => {}
                    Err(_) => break,
                }
            }
            drop(runner);
            for msg in &finished {
                if !opened.handle_job_message(msg) {
                    let message = match &msg.event {
                        JobEvent::Failed { message } => message.clone(),
                        other => format!("the analysis did not finish: {other:?}"),
                    };
                    let path = opened
                        .to_project_path(msg.job.path())
                        .unwrap_or_else(|_| msg.job.path().display().to_string());
                    analysis_errors.push(AnalysisError { path, message });
                }
            }
            if !pending.is_empty() {
                analysis_errors.push(AnalysisError {
                    path: String::new(),
                    message: "the music analysis stopped unexpectedly".into(),
                });
            }
        }

        let mut session = self.lock();
        opened.continue_revisions(session.revision());
        *session = opened;
        let mut result = ProjectResult::of(&session);
        result.missing = missing
            .into_iter()
            .map(|m| MissingOut {
                kind: m.kind,
                path: m.project_path,
            })
            .collect();
        result.analysis_errors = analysis_errors;
        json_result(&result)
    }
}

#[tool_router(router = project_router, vis = "pub(crate)")]
impl KwikvidServer {
    #[tool(
        description = "Return the kwikvid session as JSON: project (media library, \
        sections with music, pins, excludes and the edit), can_undo, can_redo, dirty, \
        media_root and revision. Times are microseconds."
    )]
    fn project_state(&self) -> Result<CallToolResult, ErrorData> {
        Ok(CallToolResult::success(vec![ContentBlock::text(
            snapshot_json(self)?,
        )]))
    }

    #[tool(
        description = "Start a new, empty, untitled project in place of the current one \
        (unsaved changes are discarded; no undo). Returns file (null), missing, revision, \
        can_undo, can_redo, dirty and the edit summary."
    )]
    fn project_new(&self) -> Result<CallToolResult, ErrorData> {
        self.project_new_impl()
    }

    #[tool(
        description = "Save the project to its file (the one it was opened from or last \
        saved as). An error while the project has no file: use project_save_as."
    )]
    fn project_save(&self) -> Result<CallToolResult, ErrorData> {
        self.project_save_impl()
    }

    #[tool(
        description = "Save the project to a .kwikvid file directly in the project root \
        (appended when the name has no extension) and make it the project's file. Returns \
        file, revision, dirty (false) and the edit summary."
    )]
    fn project_save_as(
        &self,
        Parameters(p): Parameters<ProjectPathParams>,
    ) -> Result<CallToolResult, ErrorData> {
        self.project_save_as_impl(&p.path)
    }

    #[tool(
        description = "Open a .kwikvid project file from the project root in place of the \
        current project (unsaved changes are discarded). Re-analyses its music (cached) so \
        edit_regenerate works. Returns file, missing ([{type, media_id or section, path}] \
        of files not found), analysis_errors (if any) and the write result."
    )]
    fn project_open(
        &self,
        Parameters(p): Parameters<ProjectPathParams>,
    ) -> Result<CallToolResult, ErrorData> {
        self.project_open_impl(&p.path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::*;
    use std::sync::Arc;

    #[test]
    fn project_state_is_the_session_snapshot() {
        let server = server();
        let state = ok(server.project_state());
        assert_eq!(state, serde_json::to_value(server.snapshot()).unwrap());
        for key in [
            "project",
            "can_undo",
            "can_redo",
            "dirty",
            "media_root",
            "revision",
        ] {
            assert!(state.get(key).is_some(), "{key} missing: {state}");
        }
    }

    fn server_in(root: &Path) -> KwikvidServer {
        KwikvidServer::new(root.to_owned(), ctx(Arc::new(FakeBackend::default())))
    }

    fn add_clip(server: &KwikvidServer, path: &str) {
        ok(server.media_add_impl(crate::MediaAddParams {
            path: path.into(),
            duration_us: Some(7 * SEC),
            kind: Default::default(),
            hilights_us: Vec::new(),
        }));
    }

    #[test]
    fn save_as_then_open_gives_an_equal_project() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        std::fs::write(root.join("a.mp4"), b"x").unwrap();
        let server = server_in(&root);
        add_clip(&server, "a.mp4");
        add_clip(&server, "gone.mp4");
        let before = server.project();

        let r = ok(server.project_save_as_impl("trip"));
        assert_eq!(r["file"], "trip.kwikvid");
        assert_eq!(r["dirty"], false);
        assert_eq!(r["missing"], serde_json::json!([]));
        assert!(root.join("trip.kwikvid").exists());

        ok(server.project_new_impl());
        assert!(server.project().media.is_empty());
        assert_ne!(server.project(), before);
        let revision = server.snapshot().revision;

        let abs = root.join("trip.kwikvid");
        let r = ok(server.project_open_impl(abs.to_str().unwrap()));
        assert_eq!(server.project(), before);
        assert_eq!(r["file"], "trip.kwikvid");
        assert_eq!(r["dirty"], false);
        assert_eq!(r["can_undo"], false);
        assert!(r["revision"].as_u64().unwrap() > revision, "{r}");
        assert_eq!(
            r["missing"],
            serde_json::json!([{"type": "media", "media_id": 2, "path": "gone.mp4"}])
        );
        assert_eq!(server.snapshot().media_root, root);

        // Saving again writes the same file.
        add_clip(&server, "a.mp4");
        assert_eq!(ok(server.project_save_impl())["dirty"], false);
    }

    #[test]
    fn save_without_a_file_is_an_error() {
        let e = err(server().project_save_impl());
        assert!(e.contains("project_save_as"), "{e}");
    }

    #[test]
    fn project_paths_outside_the_root_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let server = server_in(&root);
        for path in ["../p.kwikvid", "sub/p.kwikvid"] {
            assert!(server.project_save_as_impl(path).unwrap().is_error == Some(true));
            assert!(server.project_open_impl(path).unwrap().is_error == Some(true));
        }
        let elsewhere = std::env::temp_dir().join("elsewhere.kwikvid");
        let e = err(server.project_open_impl(elsewhere.to_str().unwrap()));
        assert!(e.contains("outside the project root"), "{e}");
        assert!(err(server.project_save_as_impl("p.mp4")).contains(".kwikvid"));
        assert!(err(server.project_open_impl("nope.kwikvid")).contains("couldn't be read"));
        assert!(!root.join("sub").exists());
    }
}
