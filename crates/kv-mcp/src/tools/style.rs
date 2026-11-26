//! Look tools: `style_set`, `aspect_set`, `title_set`, `title_clear`.
//!
//! Only `style_set` rebuilds the edit (the style shapes the cuts and
//! transitions); the aspect ratio and title card are applied at render time
//! and leave the edit alone (ADR 0010).

use kv_core::{AspectRatio, Command, Micros, StylePreset, TitleCard};
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::{schemars, tool, tool_router, ErrorData};
use serde::{Deserialize, Serialize};

use super::edit::section_or_default;
use crate::KwikvidServer;

/// A section's style preset, named as `kv_core::StylePreset` serialises.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum StyleParam {
    /// Hard cuts.
    Cut,
    /// Crossfades between segments.
    Smooth,
    /// Hard cuts plus 2x slow motion on HiLight segments.
    Action,
}

impl From<StyleParam> for StylePreset {
    fn from(s: StyleParam) -> Self {
        match s {
            StyleParam::Cut => StylePreset::Cut,
            StyleParam::Smooth => StylePreset::Smooth,
            StyleParam::Action => StylePreset::Action,
        }
    }
}

/// The output aspect ratio, named as `kv_core::AspectRatio` serialises.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AspectParam {
    /// 16:9 landscape.
    Landscape16x9,
    /// 9:16 portrait (vertical video).
    Portrait9x16,
    /// 1:1 square.
    Square1x1,
}

impl From<AspectParam> for AspectRatio {
    fn from(a: AspectParam) -> Self {
        match a {
            AspectParam::Landscape16x9 => AspectRatio::Landscape16x9,
            AspectParam::Portrait9x16 => AspectRatio::Portrait9x16,
            AspectParam::Square1x1 => AspectRatio::Square1x1,
        }
    }
}

/// Input of `style_set`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct StyleParams {
    /// `cut` (hard cuts), `smooth` (crossfades) or `action` (hard cuts, 2x
    /// slow motion on HiLights).
    pub style: StyleParam,
    /// Section id; defaults to the project's only section (1).
    #[serde(default)]
    pub section: Option<u64>,
}

/// Input of `aspect_set`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct AspectParams {
    /// `landscape16x9` (16:9 landscape), `portrait9x16` (9:16 portrait) or
    /// `square1x1` (1:1 square).
    pub aspect: AspectParam,
}

/// Default title card duration: 3 s, as in the desktop app
/// (`DEFAULT_TITLE_DURATION` in `crates/kv-app/ui/js/styles.js`).
pub const DEFAULT_TITLE_DURATION: Micros = Micros(3_000_000);

/// Input of `title_set`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TitleParams {
    /// Title text: 1 to 80 characters, not only whitespace.
    pub text: String,
    /// How long the title card shows, in microseconds: 500000 to 5000000
    /// (0.5 to 5 s). Default 3000000 (3 s).
    #[serde(default)]
    pub duration_us: Option<i64>,
    /// Section id; defaults to the project's only section (1).
    #[serde(default)]
    pub section: Option<u64>,
}

/// Input of `title_clear`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TitleClearParams {
    /// Section id; defaults to the project's only section (1).
    #[serde(default)]
    pub section: Option<u64>,
}

#[tool_router(router = style_router, vis = "pub(crate)")]
impl KwikvidServer {
    #[tool(
        description = "Set a section's style preset (kv-core SetStyle): cut (hard cuts), \
        smooth (crossfades) or action (hard cuts, 2x slow motion on HiLights). Rebuilds \
        the edit, replacing manual tweaks (undo brings them back)."
    )]
    fn style_set(
        &self,
        Parameters(p): Parameters<StyleParams>,
    ) -> Result<CallToolResult, ErrorData> {
        self.apply(Command::SetStyle {
            section: section_or_default(p.section),
            style: p.style.into(),
        })
    }

    #[tool(
        description = "Set the output aspect ratio (kv-core SetAspectRatio): landscape16x9 \
        is 16:9 landscape, portrait9x16 is 9:16 portrait (vertical), square1x1 is 1:1 \
        square. Does not rebuild the edit."
    )]
    fn aspect_set(
        &self,
        Parameters(p): Parameters<AspectParams>,
    ) -> Result<CallToolResult, ErrorData> {
        self.apply(Command::SetAspectRatio {
            aspect: p.aspect.into(),
        })
    }

    #[tool(
        description = "Set a section's title card (kv-core SetTitleCard): `text` 1-80 \
        characters, shown for `duration_us` (0.5-5 s, default 3 s) at the start. \
        Replaces an existing title. Does not rebuild the edit."
    )]
    fn title_set(
        &self,
        Parameters(p): Parameters<TitleParams>,
    ) -> Result<CallToolResult, ErrorData> {
        self.apply(Command::SetTitleCard {
            section: section_or_default(p.section),
            title: Some(TitleCard {
                text: p.text,
                duration: p.duration_us.map(Micros).unwrap_or(DEFAULT_TITLE_DURATION),
            }),
        })
    }

    #[tool(
        description = "Remove a section's title card (kv-core SetTitleCard with no title). \
        Does not rebuild the edit."
    )]
    fn title_clear(
        &self,
        Parameters(p): Parameters<TitleClearParams>,
    ) -> Result<CallToolResult, ErrorData> {
        self.apply(Command::SetTitleCard {
            section: section_or_default(p.section),
            title: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::*;
    use crate::tools::edit::tests::edited_server;
    use kv_core::{Project, Transition};

    #[test]
    fn param_enums_serialise_like_kv_core() {
        for s in [StyleParam::Cut, StyleParam::Smooth, StyleParam::Action] {
            assert_eq!(
                serde_json::to_value(s).unwrap(),
                serde_json::to_value(StylePreset::from(s)).unwrap()
            );
        }
        for a in [
            AspectParam::Landscape16x9,
            AspectParam::Portrait9x16,
            AspectParam::Square1x1,
        ] {
            assert_eq!(
                serde_json::to_value(a).unwrap(),
                serde_json::to_value(AspectRatio::from(a)).unwrap()
            );
        }
    }

    #[test]
    fn style_set_smooth_rebuilds_with_crossfades() {
        let server = edited_server();
        let r = ok(server.style_set(Parameters(StyleParams {
            style: StyleParam::Smooth,
            section: None,
        })));
        assert_eq!(r["regen_error"], serde_json::Value::Null, "{r}");
        let section = &server.project().sections[0];
        assert_eq!(section.style, StylePreset::Smooth);
        assert!(
            section
                .edit
                .segments
                .iter()
                .skip(1)
                .any(|s| matches!(s.transition, Transition::Crossfade { .. })),
            "{:?}",
            section.edit
        );

        // One undo step brings back the hard-cut edit.
        ok(server.edit_undo());
        let section = &server.project().sections[0];
        assert_eq!(section.style, StylePreset::Cut);
        assert!(section
            .edit
            .segments
            .iter()
            .all(|s| s.transition == Transition::Cut));
    }

    #[test]
    fn aspect_set_changes_the_setting_not_the_edit() {
        let server = edited_server();
        let edit = server.project().sections[0].edit.clone();
        ok(server.aspect_set(Parameters(AspectParams {
            aspect: AspectParam::Portrait9x16,
        })));
        let project = server.project();
        assert_eq!(project.settings.aspect_ratio, AspectRatio::Portrait9x16);
        assert_eq!(project.sections[0].edit, edit);
    }

    #[test]
    fn title_set_and_clear() {
        let server = server();
        let set = |text: &str, duration_us| {
            server.title_set(Parameters(TitleParams {
                text: text.into(),
                duration_us,
                section: None,
            }))
        };
        ok(set("Day one", None));
        assert_eq!(
            server.project().sections[0].title,
            Some(TitleCard {
                text: "Day one".into(),
                duration: DEFAULT_TITLE_DURATION,
            })
        );
        ok(set("Day two", Some(1_500_000)));
        let title = server.project().sections[0].title.clone().unwrap();
        assert_eq!(title.duration, Micros(1_500_000));

        let before = server.snapshot();
        let msg = err(set(&"x".repeat(TitleCard::MAX_CHARS + 1), None));
        assert_eq!(msg, "The title must have some text and not be too long.");
        let msg = err(set("   ", None));
        assert_eq!(msg, "The title must have some text and not be too long.");
        let msg = err(set("ok", Some(6 * SEC)));
        assert_eq!(msg, "That title duration is too short or too long.");
        assert_eq!(server.snapshot(), before);
        // Exactly the maximum length is fine.
        ok(set(&"x".repeat(TitleCard::MAX_CHARS), None));

        ok(server.title_clear(Parameters(TitleClearParams { section: None })));
        assert_eq!(server.project().sections[0].title, None);
        let msg = err(server.title_clear(Parameters(TitleClearParams { section: Some(9) })));
        assert_eq!(msg, "That part of the project no longer exists.");
        assert_eq!(
            server.project().sections[0].id,
            Project::DEFAULT_SECTION,
            "the default section is section 1"
        );
    }
}
