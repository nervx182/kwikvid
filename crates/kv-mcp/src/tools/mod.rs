//! The MCP tools, one module per area. Each module has its own
//! `#[tool_router(router = <area>_router)]` impl on [`KwikvidServer`];
//! [`KwikvidServer::new`] adds the routers together.
//!
//! - [`project`] - `project_state`.
//! - [`media`] - `media_add`, `media_remove`.
//! - [`edit`] - pin, exclude and their inverses, undo, redo; tweaks: move
//!   segment, nudge or move a cut, clip audio, music volume.
//! - [`style`] - style preset, aspect ratio, title card.
//!
//! Shared helpers (result shapes, errors, path handling) live here.

use std::path::{Component, Path, PathBuf};

use kv_core::Project;
use kv_session::regen::RegenSnapshot;
use kv_session::Snapshot;
use rmcp::model::{CallToolResult, ContentBlock};
use rmcp::ErrorData;
use serde::Serialize;

use crate::KwikvidServer;

pub mod edit;
pub mod media;
pub mod project;
pub mod style;

/// What every write tool returns (see `crates/kv-mcp/README.md`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WriteResult {
    /// The session revision after the call.
    pub revision: u64,
    pub can_undo: bool,
    pub can_redo: bool,
    /// The project differs from its saved (or initial) state.
    pub dirty: bool,
    /// The rebuild replaced manual tweaks of the edit (undo brings them back).
    pub manual_edits_replaced: bool,
    /// Why the edit could not be rebuilt (no music, analysis pending, ...);
    /// the command itself was applied. `null` when rebuilt or not due.
    pub regen_error: Option<String>,
    /// The edit after the call.
    pub edit: EditSummary,
    /// The new item's id (`media_add` only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub media_id: Option<u64>,
}

impl WriteResult {
    /// The result of an apply.
    pub fn from_regen(r: &RegenSnapshot, media_id: Option<u64>) -> Self {
        WriteResult {
            manual_edits_replaced: r.manual_edits_replaced,
            regen_error: r.regen_error.clone(),
            media_id,
            ..Self::from_snapshot(&r.snapshot)
        }
    }

    /// The result of an undo or redo (no rebuild happens).
    pub fn from_snapshot(s: &Snapshot) -> Self {
        WriteResult {
            revision: s.revision,
            can_undo: s.can_undo,
            can_redo: s.can_redo,
            dirty: s.dirty,
            manual_edits_replaced: false,
            regen_error: None,
            edit: EditSummary::of(&s.project),
            media_id: None,
        }
    }
}

/// The size of the project's edit, summed over its sections.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct EditSummary {
    /// Number of segments (cuts) in the edit.
    pub segments: usize,
    /// Total timeline length in microseconds (each section's last segment
    /// end, so crossfades are not counted twice).
    pub length_us: i64,
}

impl EditSummary {
    pub fn of(project: &Project) -> Self {
        let mut summary = EditSummary {
            segments: 0,
            length_us: 0,
        };
        for section in &project.sections {
            let segs = &section.edit.segments;
            summary.segments += segs.len();
            let end = segs
                .iter()
                .map(|s| s.timeline_start.0.saturating_add(s.timeline_len().0))
                .max()
                .unwrap_or(0);
            summary.length_us = summary.length_us.saturating_add(end);
        }
        summary
    }
}

/// A successful result holding `value` as JSON text.
pub(crate) fn json_result<T: Serialize>(value: &T) -> Result<CallToolResult, ErrorData> {
    let text =
        serde_json::to_string(value).map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
    Ok(CallToolResult::success(vec![ContentBlock::text(text)]))
}

/// An error result (`isError: true`) the model can read.
pub(crate) fn tool_error(msg: impl Into<String>) -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(msg.into())])
}

/// The session snapshot as pretty JSON (`project_state`, `project://current`).
pub(crate) fn snapshot_json(server: &KwikvidServer) -> Result<String, ErrorData> {
    serde_json::to_string_pretty(&server.snapshot())
        .map_err(|e| ErrorData::internal_error(e.to_string(), None))
}

/// A user-supplied path as a path relative to `root`, refusing anything
/// that leaves it: an absolute path outside `root`, `..`, or an empty path.
/// Lexical; the server must never read or write outside `root`.
pub(crate) fn relative_path(root: &Path, path: &str) -> Result<PathBuf, String> {
    let p = Path::new(path);
    let rel = if p.is_absolute() {
        p.strip_prefix(root).map_err(|_| {
            format!("{path:?} is outside the project root {root:?}; pass a path inside it")
        })?
    } else {
        p
    };
    let mut out = PathBuf::new();
    for c in rel.components() {
        match c {
            Component::Normal(s) => out.push(s),
            Component::CurDir => {}
            _ => return Err(format!("{path:?} must not contain '..' or a root")),
        }
    }
    if out.as_os_str().is_empty() {
        return Err("path is empty".to_owned());
    }
    Ok(out)
}

impl KwikvidServer {
    /// A user-supplied path as a project path (see
    /// [`kv_session::Session::to_project_path`]), after [`relative_path`]
    /// checked that it stays inside the root.
    pub(crate) fn project_path(&self, path: &str) -> Result<String, String> {
        let root = self.root();
        let rel = relative_path(&root, path)?;
        self.lock()
            .to_project_path(&root.join(rel))
            .map_err(|e| e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::*;

    #[test]
    fn paths_are_made_project_relative() {
        let server = server();
        let abs = root().join("x").join("y.mp4");
        assert_eq!(
            server.project_path(abs.to_str().unwrap()).unwrap(),
            "x/y.mp4"
        );
        assert_eq!(server.project_path("./a/b.mp4").unwrap(), "a/b.mp4");
        assert!(server.project_path("../escape.mp4").is_err());
        assert!(server.project_path("a/../../escape.mp4").is_err());
        assert!(server.project_path("").is_err());
        assert!(server.project_path(".").is_err());
        let outside = std::env::temp_dir().join("elsewhere.mp4");
        assert!(server.project_path(outside.to_str().unwrap()).is_err());
    }

    #[test]
    fn edit_summary_counts_segments_and_length() {
        assert_eq!(
            EditSummary::of(&Project::new(kv_core::ProjectId(1))),
            EditSummary {
                segments: 0,
                length_us: 0
            }
        );
    }
}
