//! Edit tools: pin / exclude ranges and their inverses, undo, redo.

use kv_core::{Command, MediaId, Micros, Project, RangeRef, SectionId};
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::{schemars, tool, tool_router, ErrorData};
use serde::{Deserialize, Serialize};

use super::{json_result, tool_error, WriteResult};
use crate::KwikvidServer;

/// Input of the range tools: a half-open range `[start_us, end_us)` of one
/// media item.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RangeParams {
    /// Id of the media item (see `media_add` or `project_state`).
    pub media_id: u64,
    /// Range start in microseconds from the start of the media (inclusive).
    pub start_us: i64,
    /// Range end in microseconds from the start of the media (exclusive).
    pub end_us: i64,
    /// Section id; defaults to the project's only section (1).
    #[serde(default)]
    pub section: Option<u64>,
}

impl RangeParams {
    fn section(&self) -> SectionId {
        self.section
            .map(SectionId)
            .unwrap_or(Project::DEFAULT_SECTION)
    }

    fn range(&self) -> RangeRef {
        RangeRef {
            media_id: MediaId(self.media_id),
            start: Micros(self.start_us),
            end: Micros(self.end_us),
        }
    }
}

#[tool_router(router = edit_router, vis = "pub(crate)")]
impl KwikvidServer {
    #[tool(
        description = "Force a range of a clip into the edit (kv-core PinRange) and \
        rebuild the edit. Rejected if it overlaps a pinned or excluded range."
    )]
    pub(crate) fn edit_pin_range(
        &self,
        Parameters(p): Parameters<RangeParams>,
    ) -> Result<CallToolResult, ErrorData> {
        self.apply(Command::PinRange {
            section: p.section(),
            range: p.range(),
        })
    }

    #[tool(
        description = "Stop forcing a pinned range into the edit (kv-core UnpinRange) and \
        rebuild the edit. The range must equal an existing pin."
    )]
    fn edit_unpin_range(
        &self,
        Parameters(p): Parameters<RangeParams>,
    ) -> Result<CallToolResult, ErrorData> {
        self.apply(Command::UnpinRange {
            section: p.section(),
            range: p.range(),
        })
    }

    #[tool(
        description = "Force a range of a clip out of the edit (kv-core ExcludeRange) and \
        rebuild the edit; use 0..duration to exclude a whole clip. Rejected if it overlaps \
        a pinned or excluded range."
    )]
    fn edit_exclude_range(
        &self,
        Parameters(p): Parameters<RangeParams>,
    ) -> Result<CallToolResult, ErrorData> {
        self.apply(Command::ExcludeRange {
            section: p.section(),
            range: p.range(),
        })
    }

    #[tool(
        description = "Allow an excluded range back into the edit (kv-core UnexcludeRange) \
        and rebuild the edit. The range must equal an existing exclude."
    )]
    fn edit_unexclude_range(
        &self,
        Parameters(p): Parameters<RangeParams>,
    ) -> Result<CallToolResult, ErrorData> {
        self.apply(Command::UnexcludeRange {
            section: p.section(),
            range: p.range(),
        })
    }

    #[tool(
        description = "Undo the last action (with the edit rebuild it caused). \
        An error result when there is nothing to undo."
    )]
    fn edit_undo(&self) -> Result<CallToolResult, ErrorData> {
        match self.lock().undo() {
            Ok(s) => json_result(&WriteResult::from_snapshot(&s)),
            Err(e) => Ok(tool_error(e.to_string())),
        }
    }

    #[tool(description = "Redo the last undone action. \
        An error result when there is nothing to redo.")]
    fn edit_redo(&self) -> Result<CallToolResult, ErrorData> {
        match self.lock().redo() {
            Ok(s) => json_result(&WriteResult::from_snapshot(&s)),
            Err(e) => Ok(tool_error(e.to_string())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::*;
    use crate::MediaAddParams;
    use kv_core::MusicRef;

    fn range(media_id: u64, start_s: i64, end_s: i64) -> Parameters<RangeParams> {
        Parameters(RangeParams {
            media_id,
            start_us: start_s * SEC,
            end_us: end_s * SEC,
            section: None,
        })
    }

    fn add_clips(server: &KwikvidServer) {
        for name in ["a.mp4", "b.mp4"] {
            ok(server.media_add_impl(MediaAddParams {
                path: name.into(),
                duration_us: Some(10 * SEC),
                kind: crate::MediaKindParam::Video,
                hilights_us: vec![3 * SEC],
            }));
        }
    }

    #[test]
    fn rejected_command_is_a_tool_error_and_changes_nothing() {
        let server = server();
        let before = server.snapshot();
        let msg = err(server.apply(Command::PinRange {
            section: Project::DEFAULT_SECTION,
            range: RangeRef {
                media_id: MediaId(9),
                start: Micros(0),
                end: Micros(1),
            },
        }));
        assert!(msg.contains("no longer in the project"), "{msg}");
        assert_eq!(server.snapshot(), before);

        add_clips(&server);
        ok(server.edit_pin_range(range(1, 1, 3)));
        let before = server.snapshot();
        let msg = err(server.edit_exclude_range(range(1, 2, 4)));
        assert!(msg.contains("overlaps"), "{msg}");
        let msg = err(server.edit_unexclude_range(range(1, 2, 4)));
        assert!(msg.contains("isn't excluded"), "{msg}");
        assert_eq!(server.snapshot(), before);
    }

    #[test]
    fn pin_then_unpin_restores_the_state() {
        let server = server();
        add_clips(&server);
        let before = server.project();
        let r = ok(server.edit_pin_range(range(1, 1, 3)));
        // No music yet: the pin applies, the edit is not rebuilt.
        assert!(r["regen_error"].is_string(), "{r}");
        assert_eq!(server.project().sections[0].pins.len(), 1);
        let r = ok(server.edit_unpin_range(range(1, 1, 3)));
        assert_eq!(r["can_undo"], true);
        assert_eq!(server.project(), before);
        let msg = err(server.edit_unpin_range(range(1, 1, 3)));
        assert!(msg.contains("isn't forced in"), "{msg}");
    }

    #[test]
    fn exclude_then_undo_then_redo() {
        let server = server();
        add_clips(&server);
        let before = server.project();
        ok(server.edit_exclude_range(range(2, 0, 10)));
        let excluded = server.project();
        assert_eq!(excluded.sections[0].excludes.len(), 1);

        let r = ok(server.edit_undo());
        assert_eq!(r["can_redo"], true);
        assert_eq!(server.project(), before);

        let r = ok(server.edit_redo());
        assert_eq!(r["can_redo"], false);
        assert_eq!(r["can_undo"], true);
        assert_eq!(server.project(), excluded);
        assert_eq!(err(server.edit_redo()), "Nothing to redo.");

        ok(server.edit_unexclude_range(range(2, 0, 10)));
        assert_eq!(server.project(), before);

        // Two adds, exclude, unexclude.
        for _ in 0..4 {
            ok(server.edit_undo());
        }
        assert_eq!(err(server.edit_undo()), "Nothing to undo.");
    }

    #[test]
    fn pin_with_music_and_analysis_regenerates_the_edit() {
        let server = server();
        add_clips(&server);
        // Music without its analysis: the edit cannot be built yet.
        let r = ok(server.apply(Command::SetMusic {
            section: Project::DEFAULT_SECTION,
            music: Some(MusicRef {
                path: "song.mp3".into(),
                duration: Micros(12 * SEC),
            }),
        }));
        assert!(r["regen_error"].is_string(), "{r}");
        assert_eq!(r["edit"]["segments"], 0);

        server
            .lock()
            .set_music_analysis("song.mp3".into(), music(12));
        let r = ok(server.edit_pin_range(range(2, 5, 7)));
        assert_eq!(r["regen_error"], serde_json::Value::Null, "{r}");
        assert_eq!(r["manual_edits_replaced"], false);
        let segments = r["edit"]["segments"].as_u64().unwrap();
        assert!(segments > 0, "{r}");
        assert!(r["edit"]["length_us"].as_i64().unwrap() > 0, "{r}");
        let edit = &server.project().sections[0].edit;
        assert_eq!(edit.segments.len() as u64, segments);
        // The pinned range is in the edit.
        assert!(
            edit.segments.iter().any(|s| s.media_id == MediaId(2)
                && s.src_in <= Micros(5 * SEC)
                && s.src_out >= Micros(7 * SEC)),
            "{edit:?}"
        );

        // One undo removes the pin and the rebuilt edit together.
        let r = ok(server.edit_undo());
        assert_eq!(r["edit"]["segments"], 0);
        assert!(server.project().sections[0].pins.is_empty());
    }
}
