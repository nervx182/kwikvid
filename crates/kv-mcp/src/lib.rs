//! kwikvid MCP server.
//!
//! [`KwikvidServer`] holds one in-memory [`kv_core::Project`] (inside a
//! [`kv_core::History`] for undo) and exposes it to MCP clients (Claude
//! Desktop, Claude Code, ...) through [`rmcp`]. Every write tool maps 1:1 onto
//! a [`kv_core::Command`], so the MCP server, the CLI and the desktop app share
//! one API (see `docs/architecture.md`).
//!
//! Tools:
//! - `project_state`: the project as JSON.
//! - `media_add`: [`Command::AddMedia`]; probes the duration through a
//!   [`MediaBackend`] unless `duration_us` is given.
//! - `edit_pin_range`: [`Command::PinRange`].
//! - `edit_exclude_range`: [`Command::ExcludeRange`].
//! - `edit_undo`: [`History::undo`].
//!
//! Resource: [`PROJECT_URI`] (`project://current`), the same JSON as
//! `project_state`.
//!
//! A rejected command is reported as a tool result with `isError: true` and
//! the [`kv_core::CoreError`] message, so the model can read and correct it.

use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use kv_core::{
    Command, History, MediaId, MediaItem, MediaKind, Micros, Project, ProjectId, RangeRef,
    SectionId,
};
use kv_media::MediaBackend;
use rmcp::handler::server::{router::tool::ToolRouter, wrapper::Parameters};
use rmcp::model::{
    CallToolResult, ContentBlock, Implementation, ListResourcesResult, PaginatedRequestParams,
    ReadResourceRequestParams, ReadResourceResponse, ReadResourceResult, Resource,
    ResourceContents, ServerCapabilities, ServerConfig,
};
use rmcp::service::RequestContext;
use rmcp::{schemars, tool, tool_handler, tool_router, ErrorData, RoleServer, ServerHandler};
use serde::{Deserialize, Serialize};

/// URI of the resource holding the current project JSON.
pub const PROJECT_URI: &str = "project://current";

/// Kind of media for `media_add`.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum MediaKindParam {
    #[default]
    Video,
    Photo,
}

impl From<MediaKindParam> for MediaKind {
    fn from(k: MediaKindParam) -> Self {
        match k {
            MediaKindParam::Video => MediaKind::Video,
            MediaKindParam::Photo => MediaKind::Photo,
        }
    }
}

/// Input of `media_add`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct MediaAddParams {
    /// Path of the clip or photo: relative to the project root (the server's
    /// `--root`, default its working directory), or absolute inside it.
    pub path: String,
    /// Duration in microseconds. When omitted the file is probed; give it to
    /// add media without reading the file (tests, placeholders).
    #[serde(default)]
    pub duration_us: Option<i64>,
    /// Video (default) or photo.
    #[serde(default)]
    pub kind: MediaKindParam,
    /// HiLight tags in microseconds from the start of the media.
    #[serde(default)]
    pub hilights_us: Vec<i64>,
}

/// Input of `edit_pin_range` and `edit_exclude_range`: a half-open range
/// `[start_us, end_us)` of one media item.
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

/// The MCP server: one in-memory project with undo history.
#[derive(Clone)]
pub struct KwikvidServer {
    history: Arc<Mutex<History>>,
    root: PathBuf,
    backend: Option<Arc<dyn MediaBackend>>,
    tool_router: ToolRouter<Self>,
}

impl std::fmt::Debug for KwikvidServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KwikvidServer")
            .field("root", &self.root)
            .field("has_backend", &self.backend.is_some())
            .finish_non_exhaustive()
    }
}

impl KwikvidServer {
    /// A server holding an empty project. `root` is the directory media paths
    /// are relative to; `backend` probes media when `media_add` gets no
    /// duration (`None` makes the duration mandatory).
    pub fn new(root: PathBuf, backend: Option<Arc<dyn MediaBackend>>) -> Self {
        KwikvidServer {
            history: Arc::new(Mutex::new(History::new(Project::new(ProjectId(1))))),
            root,
            backend,
            tool_router: Self::tool_router(),
        }
    }

    /// A copy of the current project.
    pub fn project(&self) -> Project {
        self.lock().project().clone()
    }

    fn lock(&self) -> MutexGuard<'_, History> {
        // A panic while holding the lock cannot leave History half-updated
        // (apply swaps in a fully built project), so poisoning is harmless.
        self.history.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn project_json(&self) -> Result<String, ErrorData> {
        serde_json::to_string_pretty(self.lock().project())
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))
    }

    /// Apply a command through the history and report the event (or the
    /// rejection) as a tool result.
    fn run(&self, command: Command) -> Result<CallToolResult, ErrorData> {
        tracing::debug!(?command, "applying command");
        match self.lock().apply(command) {
            Ok(event) => json_result(&event),
            Err(e) => Ok(CallToolResult::error(vec![ContentBlock::text(
                e.to_string(),
            )])),
        }
    }

    /// Turn a user-supplied path into a project-relative, `/`-separated one.
    fn relative_path(&self, path: &str) -> Result<String, String> {
        let p = Path::new(path);
        let rel = if p.is_absolute() {
            p.strip_prefix(&self.root).map_err(|_| {
                format!(
                    "{path:?} is outside the project root {:?}; pass a path inside it",
                    self.root
                )
            })?
        } else {
            p
        };
        let mut parts = Vec::new();
        for c in rel.components() {
            match c {
                Component::Normal(s) => parts.push(s.to_string_lossy().into_owned()),
                Component::CurDir => {}
                _ => return Err(format!("{path:?} must not contain '..' or a root")),
            }
        }
        if parts.is_empty() {
            return Err("path is empty".to_owned());
        }
        Ok(parts.join("/"))
    }

    fn media_add_impl(&self, p: MediaAddParams) -> Result<CallToolResult, ErrorData> {
        let path = match self.relative_path(&p.path) {
            Ok(path) => path,
            Err(msg) => return Ok(tool_error(msg)),
        };
        let duration = match p.duration_us {
            Some(us) => Micros(us),
            None => {
                let Some(backend) = &self.backend else {
                    return Ok(tool_error(
                        "no media backend available to probe the file; pass duration_us",
                    ));
                };
                match backend.probe(&self.root.join(&path)) {
                    Ok(info) => info.duration,
                    Err(e) => return Ok(tool_error(e.to_string())),
                }
            }
        };
        let mut history = self.lock();
        let item = MediaItem {
            id: history.project().next_media_id(),
            path,
            duration,
            kind: p.kind.into(),
            hilights: p.hilights_us.into_iter().map(Micros).collect(),
        };
        tracing::debug!(?item, "adding media");
        match history.apply(Command::AddMedia { item }) {
            Ok(event) => json_result(&event),
            Err(e) => Ok(tool_error(e.to_string())),
        }
    }
}

#[tool_router]
impl KwikvidServer {
    #[tool(
        description = "Return the current kwikvid project (media library, sections, \
        pins, excludes and edit decision list) as JSON. Times are microseconds."
    )]
    fn project_state(&self) -> Result<CallToolResult, ErrorData> {
        Ok(CallToolResult::success(vec![ContentBlock::text(
            self.project_json()?,
        )]))
    }

    #[tool(
        description = "Add a clip or photo to the project library (kv-core AddMedia). \
        Returns the media_added event with the new media_id."
    )]
    fn media_add(
        &self,
        Parameters(p): Parameters<MediaAddParams>,
    ) -> Result<CallToolResult, ErrorData> {
        self.media_add_impl(p)
    }

    #[tool(
        description = "Force a range of a clip into the edit (kv-core PinRange). \
        Rejected if it overlaps an excluded range."
    )]
    fn edit_pin_range(
        &self,
        Parameters(p): Parameters<RangeParams>,
    ) -> Result<CallToolResult, ErrorData> {
        self.run(Command::PinRange {
            section: p.section(),
            range: p.range(),
        })
    }

    #[tool(
        description = "Force a range of a clip out of the edit (kv-core ExcludeRange); \
        use 0..duration to exclude a whole clip. Rejected if it overlaps a pinned range."
    )]
    fn edit_exclude_range(
        &self,
        Parameters(p): Parameters<RangeParams>,
    ) -> Result<CallToolResult, ErrorData> {
        self.run(Command::ExcludeRange {
            section: p.section(),
            range: p.range(),
        })
    }

    #[tool(
        description = "Undo the last applied edit. Returns the undone command, \
        or an error result when there is nothing to undo."
    )]
    fn edit_undo(&self) -> Result<CallToolResult, ErrorData> {
        let mut history = self.lock();
        match history.undo() {
            Some(cmd) => json_result(&serde_json::json!({ "undone": cmd })),
            None => Ok(tool_error("nothing to undo")),
        }
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for KwikvidServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_resources()
                .build(),
        )
        .with_server_info(Implementation::new(
            "kwikvid-mcp",
            env!("CARGO_PKG_VERSION"),
        ))
        .with_instructions(
            "kwikvid video editor. Add clips with media_add, steer the edit with \
             edit_pin_range / edit_exclude_range, undo with edit_undo, and read the \
             project with project_state or the project://current resource. \
             All times are integer microseconds.",
        )
    }

    async fn list_resources(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, ErrorData> {
        Ok(ListResourcesResult::with_all_items(vec![Resource::new(
            PROJECT_URI,
            "current project",
        )
        .with_description("The in-memory kwikvid project as JSON")
        .with_mime_type("application/json")]))
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResponse, ErrorData> {
        if request.uri != PROJECT_URI {
            return Err(ErrorData::resource_not_found(
                format!("no resource at {}", request.uri),
                None,
            ));
        }
        let contents = ResourceContents::text(self.project_json()?, request.uri)
            .with_mime_type("application/json");
        Ok(ReadResourceResult::new(vec![contents]).into())
    }
}

fn json_result<T: Serialize>(value: &T) -> Result<CallToolResult, ErrorData> {
    let text =
        serde_json::to_string(value).map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
    Ok(CallToolResult::success(vec![ContentBlock::text(text)]))
}

fn tool_error(msg: impl Into<String>) -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(msg.into())])
}

#[cfg(test)]
mod tests {
    use super::*;
    use kv_media::{MediaError, MediaInfo};

    /// Probes every file as 7 s long and records the path it was asked for.
    struct FakeBackend(Mutex<Vec<PathBuf>>);

    impl MediaBackend for FakeBackend {
        fn probe(&self, path: &Path) -> kv_media::Result<MediaInfo> {
            self.0.lock().unwrap().push(path.to_owned());
            if path.ends_with("missing.mp4") {
                return Err(MediaError::NoStreams(path.to_owned()));
            }
            Ok(MediaInfo {
                duration: Micros(7_000_000),
                container: "fake".into(),
                video: None,
                audio: None,
                creation_time: None,
            })
        }
    }

    fn root() -> PathBuf {
        std::env::temp_dir().join("kv-mcp-root")
    }

    fn text(r: &CallToolResult) -> String {
        r.content[0].as_text().unwrap().text.clone()
    }

    fn add(path: &str, duration_us: Option<i64>) -> MediaAddParams {
        MediaAddParams {
            path: path.into(),
            duration_us,
            kind: MediaKindParam::Video,
            hilights_us: vec![],
        }
    }

    #[test]
    fn media_add_probes_relative_to_root_when_no_duration() {
        let backend = Arc::new(FakeBackend(Mutex::new(vec![])));
        let server = KwikvidServer::new(root(), Some(backend.clone()));
        let r = server.media_add_impl(add("clips/a.mp4", None)).unwrap();
        assert_ne!(r.is_error, Some(true), "{}", text(&r));
        assert_eq!(*backend.0.lock().unwrap(), [root().join("clips/a.mp4")]);
        let p = server.project();
        assert_eq!(p.media[0].duration, Micros(7_000_000));
        assert_eq!(p.media[0].path, "clips/a.mp4");
    }

    #[test]
    fn media_add_reports_probe_failure_and_missing_backend() {
        let backend = Arc::new(FakeBackend(Mutex::new(vec![])));
        let server = KwikvidServer::new(root(), Some(backend));
        let r = server.media_add_impl(add("missing.mp4", None)).unwrap();
        assert_eq!(r.is_error, Some(true));
        assert!(server.project().media.is_empty());

        let server = KwikvidServer::new(root(), None);
        let r = server.media_add_impl(add("a.mp4", None)).unwrap();
        assert_eq!(r.is_error, Some(true));
        assert!(text(&r).contains("duration_us"));
    }

    #[test]
    fn media_add_override_skips_probe() {
        let backend = Arc::new(FakeBackend(Mutex::new(vec![])));
        let server = KwikvidServer::new(root(), Some(backend.clone()));
        let r = server.media_add_impl(add("a.mp4", Some(3))).unwrap();
        assert_ne!(r.is_error, Some(true), "{}", text(&r));
        assert!(backend.0.lock().unwrap().is_empty());
        assert_eq!(server.project().media[0].duration, Micros(3));
    }

    #[test]
    fn paths_are_made_project_relative() {
        let server = KwikvidServer::new(root(), None);
        let abs = root().join("x").join("y.mp4");
        assert_eq!(
            server.relative_path(abs.to_str().unwrap()).unwrap(),
            "x/y.mp4"
        );
        assert_eq!(server.relative_path("./a/b.mp4").unwrap(), "a/b.mp4");
        assert!(server.relative_path("../escape.mp4").is_err());
        assert!(server.relative_path("").is_err());
        let outside = std::env::temp_dir().join("elsewhere.mp4");
        assert!(server.relative_path(outside.to_str().unwrap()).is_err());
    }

    #[test]
    fn rejected_command_is_a_tool_error_and_changes_nothing() {
        let server = KwikvidServer::new(root(), None);
        let before = server.project();
        let r = server
            .run(Command::PinRange {
                section: Project::DEFAULT_SECTION,
                range: RangeRef {
                    media_id: MediaId(9),
                    start: Micros(0),
                    end: Micros(1),
                },
            })
            .unwrap();
        assert_eq!(r.is_error, Some(true));
        assert!(text(&r).contains("unknown media"));
        assert_eq!(server.project(), before);
    }
}
