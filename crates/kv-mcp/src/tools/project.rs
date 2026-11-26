//! Project tools: `project_state` (later: save, open).

use rmcp::model::{CallToolResult, ContentBlock};
use rmcp::{tool, tool_router, ErrorData};

use super::snapshot_json;
use crate::KwikvidServer;

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
}

#[cfg(test)]
mod tests {
    use crate::test_support::*;

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
}
