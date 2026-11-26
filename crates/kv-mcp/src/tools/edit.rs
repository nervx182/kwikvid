//! Edit tools: pin / exclude ranges and their inverses, undo, redo, and
//! the manual tweaks (reorder segments, move or nudge cuts, clip audio,
//! music volume). Tweaks never rebuild the edit (ADR 0010).

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

/// A tool's optional `section`, defaulting to the project's only section.
pub(crate) fn section_or_default(section: Option<u64>) -> SectionId {
    section.map(SectionId).unwrap_or(Project::DEFAULT_SECTION)
}

impl RangeParams {
    fn section(&self) -> SectionId {
        section_or_default(self.section)
    }

    fn range(&self) -> RangeRef {
        RangeRef {
            media_id: MediaId(self.media_id),
            start: Micros(self.start_us),
            end: Micros(self.end_us),
        }
    }
}

/// Input of `edit_move_segment`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct MoveSegmentParams {
    /// Index (from 0) of the segment to move, in the section's edit.
    pub from: usize,
    /// Index (from 0) the segment ends up at.
    pub to: usize,
    /// Section id; defaults to the project's only section (1).
    #[serde(default)]
    pub section: Option<u64>,
}

/// Input of `edit_nudge_cut`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct NudgeCutParams {
    /// The cut between segments `cut - 1` and `cut` (1 to segments - 1).
    pub cut: usize,
    /// Beats to move the cut by, from the beat nearest to it: positive is
    /// later, negative earlier, 0 snaps it to that nearest beat.
    pub beats: i32,
    /// Section id; defaults to the project's only section (1).
    #[serde(default)]
    pub section: Option<u64>,
}

/// Input of `edit_move_cut`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct MoveCutParams {
    /// The cut between segments `cut - 1` and `cut` (1 to segments - 1).
    pub cut: usize,
    /// New timeline time of the cut, in microseconds.
    pub to_us: i64,
    /// Section id; defaults to the project's only section (1).
    #[serde(default)]
    pub section: Option<u64>,
}

/// Input of `clip_audio_set`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ClipAudioParams {
    /// Id of the media item (see `media_add` or `project_state`).
    pub media_id: u64,
    /// Play the clip's own audio in the edit (true) or mute it (false).
    pub enabled: bool,
}

/// Input of `music_volume_set`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct MusicVolumeParams {
    /// Music volume, 0 (silent) to 100 (full) percent.
    #[schemars(range(max = 100))]
    pub percent: u32,
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
    pub(crate) fn edit_undo(&self) -> Result<CallToolResult, ErrorData> {
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

    #[tool(
        description = "Reorder the edit: move the segment at index `from` to index `to` \
        (from 0) and close the gaps (kv-core MoveSegment). A manual tweak: does not \
        rebuild the edit; a later rebuild (pin, exclude, style, music, media change) \
        replaces it."
    )]
    fn edit_move_segment(
        &self,
        Parameters(p): Parameters<MoveSegmentParams>,
    ) -> Result<CallToolResult, ErrorData> {
        self.apply(Command::MoveSegment {
            section: section_or_default(p.section),
            from: p.from,
            to: p.to,
        })
    }

    #[tool(
        description = "Move cut `cut` (between segments cut-1 and cut) by `beats` beats of \
        the music from the beat nearest to it; negative is earlier, 0 snaps it to that \
        beat (kv-session nudge_cut, applied as kv-core MoveCut). Needs the section's \
        music and its analysis. A manual tweak: does not rebuild the edit."
    )]
    fn edit_nudge_cut(
        &self,
        Parameters(p): Parameters<NudgeCutParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let section = section_or_default(p.section);
        match self.lock().nudge_cut(section, p.cut, p.beats) {
            Ok(s) => json_result(&WriteResult::from_snapshot(&s)),
            Err(e) => Ok(tool_error(e.to_string())),
        }
    }

    #[tool(
        description = "Move cut `cut` (between segments cut-1 and cut) to timeline time \
        `to_us`, off the beat grid if need be (kv-core MoveCut). Rejected if a segment \
        would get too short or run past its clip. A manual tweak: does not rebuild the \
        edit."
    )]
    fn edit_move_cut(
        &self,
        Parameters(p): Parameters<MoveCutParams>,
    ) -> Result<CallToolResult, ErrorData> {
        self.apply(Command::MoveCut {
            section: section_or_default(p.section),
            cut: p.cut,
            to: Micros(p.to_us),
        })
    }

    #[tool(
        description = "Turn a clip's own audio on or off in the edit (kv-core SetClipAudio). \
        A manual tweak: does not rebuild the edit."
    )]
    fn clip_audio_set(
        &self,
        Parameters(p): Parameters<ClipAudioParams>,
    ) -> Result<CallToolResult, ErrorData> {
        self.apply(Command::SetClipAudio {
            media_id: MediaId(p.media_id),
            enabled: p.enabled,
        })
    }

    #[tool(
        description = "Set the music volume, 0-100 percent (kv-core SetMusicVolume). \
        A manual tweak: does not rebuild the edit."
    )]
    fn music_volume_set(
        &self,
        Parameters(p): Parameters<MusicVolumeParams>,
    ) -> Result<CallToolResult, ErrorData> {
        match u8::try_from(p.percent) {
            Ok(percent) => self.apply(Command::SetMusicVolume { percent }),
            // Same message as kv-core's for 101..=255.
            Err(_) => Ok(tool_error("Music volume must be between 0 and 100%.")),
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::test_support::*;
    use crate::MediaAddParams;
    use kv_core::MusicRef;

    /// Two 10 s clips, 12 s of analysed 120 bpm music and the auto-edit
    /// built from them.
    pub(crate) fn edited_server() -> KwikvidServer {
        let server = server();
        add_clips(&server);
        server
            .lock()
            .set_music_analysis("song.mp3".into(), music(12));
        let r = ok(server.apply(Command::SetMusic {
            section: Project::DEFAULT_SECTION,
            music: Some(MusicRef {
                path: "song.mp3".into(),
                duration: Micros(12 * SEC),
            }),
        }));
        assert_eq!(r["regen_error"], serde_json::Value::Null, "{r}");
        assert!(server.project().sections[0].edit.segments.len() >= 3);
        server
    }

    fn segments(server: &KwikvidServer) -> Vec<kv_core::Segment> {
        server.project().sections[0].edit.segments.clone()
    }

    fn move_segment(from: usize, to: usize) -> Parameters<MoveSegmentParams> {
        Parameters(MoveSegmentParams {
            from,
            to,
            section: None,
        })
    }

    fn nudge(cut: usize, beats: i32) -> Parameters<NudgeCutParams> {
        Parameters(NudgeCutParams {
            cut,
            beats,
            section: None,
        })
    }

    #[test]
    fn move_segment_reorders_without_rebuilding_then_undo() {
        let server = edited_server();
        let before = server.project();
        let segs = segments(&server);
        let r = ok(server.edit_move_segment(move_segment(0, 2)));
        assert_eq!(r["manual_edits_replaced"], false);
        let moved = segments(&server);
        assert_eq!(moved.len(), segs.len());
        assert_eq!(
            (moved[2].media_id, moved[2].src_in),
            (segs[0].media_id, segs[0].src_in)
        );
        assert_eq!(
            (moved[0].media_id, moved[0].src_in),
            (segs[1].media_id, segs[1].src_in)
        );

        ok(server.edit_undo());
        assert_eq!(server.project(), before);

        let msg = err(server.edit_move_segment(move_segment(0, 99)));
        assert_eq!(msg, "That cut is no longer in the timeline.");
        assert_eq!(server.project(), before);
    }

    #[test]
    fn style_rebuild_replaces_a_manual_reorder() {
        let server = edited_server();
        ok(server.edit_move_segment(move_segment(0, 1)));
        let r = ok(server.apply(Command::SetStyle {
            section: Project::DEFAULT_SECTION,
            style: kv_core::StylePreset::Cut,
        }));
        assert_eq!(r["manual_edits_replaced"], true, "{r}");
    }

    #[test]
    fn nudge_cut_moves_to_the_next_beat() {
        let server = edited_server();
        let segs = segments(&server);
        // 120 bpm: a beat every 0.5 s; auto-edit cuts sit on beats.
        let at = segs[1].timeline_start.0;
        assert_eq!(at % (SEC / 2), 0, "cut not on a beat: {at}");
        let r = ok(server.edit_nudge_cut(nudge(1, 1)));
        assert_eq!(r["can_undo"], true);
        assert_eq!(segments(&server)[1].timeline_start, Micros(at + SEC / 2));
        ok(server.edit_nudge_cut(nudge(1, -1)));
        assert_eq!(segments(&server)[1].timeline_start, Micros(at));
        // 0 snaps to the nearest beat.
        ok(server.edit_move_cut(Parameters(MoveCutParams {
            cut: 1,
            to_us: at + 100_000,
            section: None,
        })));
        assert_eq!(segments(&server)[1].timeline_start, Micros(at + 100_000));
        ok(server.edit_nudge_cut(nudge(1, 0)));
        assert_eq!(segments(&server)[1].timeline_start, Micros(at));

        let before = server.project();
        let msg = err(server.edit_nudge_cut(nudge(0, 1)));
        assert_eq!(msg, "That cut is no longer in the timeline.");
        let msg = err(server.edit_nudge_cut(nudge(1, -1000)));
        assert!(!msg.is_empty());
        assert_eq!(server.project(), before);
    }

    #[test]
    fn nudge_cut_errors_are_tool_errors() {
        let server = edited_server();
        let set_music = |path: Option<&str>| {
            ok(server.apply(Command::SetMusic {
                section: Project::DEFAULT_SECTION,
                music: path.map(|p| MusicRef {
                    path: p.into(),
                    duration: Micros(12 * SEC),
                }),
            }))
        };
        // A short first segment: one more beat back would make it too short.
        let at = segments(&server)[1].timeline_start.0;
        ok(server.edit_move_cut(Parameters(MoveCutParams {
            cut: 1,
            to_us: SEC / 2,
            section: None,
        })));
        let msg = err(server.edit_nudge_cut(nudge(1, -1)));
        assert!(
            msg == "Moving that cut would make a clip too short."
                || msg == "There is no beat there to move the cut to.",
            "{msg}"
        );
        assert_eq!(segments(&server)[1].timeline_start, Micros(SEC / 2));
        assert!(at > 0);

        // New music without its analysis: the edit stays, the nudge waits.
        let r = set_music(Some("other.mp3"));
        assert!(r["regen_error"].is_string(), "{r}");
        assert_eq!(
            err(server.edit_nudge_cut(nudge(1, 1))),
            "Still analysing the music…"
        );
        // No music at all.
        set_music(None);
        assert_eq!(
            err(server.edit_nudge_cut(nudge(1, 1))),
            "Add music to move cuts to the beat."
        );
    }

    #[test]
    fn move_cut_off_the_grid_and_rejects_too_short() {
        let server = edited_server();
        let at = segments(&server)[1].timeline_start.0;
        ok(server.edit_move_cut(Parameters(MoveCutParams {
            cut: 1,
            to_us: at - 123_456,
            section: None,
        })));
        assert_eq!(segments(&server)[1].timeline_start, Micros(at - 123_456));
        let before = server.project();
        let msg = err(server.edit_move_cut(Parameters(MoveCutParams {
            cut: 1,
            to_us: 1,
            section: None,
        })));
        assert!(!msg.is_empty());
        assert_eq!(server.project(), before);
    }

    #[test]
    fn clip_audio_and_music_volume() {
        let server = edited_server();
        let edit = server.project().sections[0].edit.clone();
        ok(server.clip_audio_set(Parameters(ClipAudioParams {
            media_id: 2,
            enabled: true,
        })));
        ok(server.music_volume_set(Parameters(MusicVolumeParams { percent: 40 })));
        let project = server.project();
        assert!(project.media[1].clip_audio);
        assert!(!project.media[0].clip_audio);
        assert_eq!(project.settings.music_volume_percent, 40);
        // Tweaks do not rebuild the edit.
        assert_eq!(project.sections[0].edit, edit);

        for percent in [101, 300] {
            let msg = err(server.music_volume_set(Parameters(MusicVolumeParams { percent })));
            assert_eq!(msg, "Music volume must be between 0 and 100%.");
        }
        let msg = err(server.clip_audio_set(Parameters(ClipAudioParams {
            media_id: 9,
            enabled: true,
        })));
        assert!(!msg.is_empty());
        assert_eq!(server.project(), project);
        ok(server.edit_undo());
        assert_eq!(server.project().settings.music_volume_percent, 100);
    }

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
