//! kwikvid MCP server.
//!
//! [`KwikvidServer`] holds one [`kv_session::Session`] (the same project,
//! undo history and auto-regenerate logic the desktop app uses) plus a
//! [`kv_session::JobContext`] with the media backend, music analyzer and
//! cache, and exposes them to MCP clients (Claude Desktop, Claude Code, ...)
//! through [`rmcp`]. Every write tool maps onto a [`kv_core::Command`] applied
//! with [`Session::apply_and_regenerate`], so pins, excludes and media changes
//! rebuild the edit exactly as in the app (ADR 0010), and the MCP server, the
//! CLI and the desktop app share one API (see `docs/architecture.md`).
//!
//! Tools live in [`tools`], one module per area, each with its own
//! `#[tool_router]`; [`KwikvidServer::new`] adds the routers together. See
//! `crates/kv-mcp/README.md` for the tool list and result shapes.
//!
//! Resource: [`PROJECT_URI`] (`project://current`), the same
//! [`kv_session::Snapshot`] JSON as `project_state`.
//!
//! A rejected command or session error is reported as a tool result with
//! `isError: true` and a readable message, so the model can read and correct
//! it; the project is then unchanged.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use kv_core::{Command, Project};
use kv_media::{MediaBackend, MediaError, MediaInfo};
use kv_session::{JobContext, Session, Snapshot};
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::model::{
    CallToolResult, Implementation, ListResourcesResult, PaginatedRequestParams,
    ReadResourceRequestParams, ReadResourceResponse, ReadResourceResult, Resource,
    ResourceContents, ServerCapabilities, ServerConfig,
};
use rmcp::service::RequestContext;
use rmcp::{tool_handler, ErrorData, RoleServer, ServerHandler};

pub mod tools;

pub use tools::edit::RangeParams;
pub use tools::media::{MediaAddParams, MediaKindParam, MediaRemoveParams};
pub use tools::{EditSummary, WriteResult};

/// URI of the resource holding the current session snapshot JSON.
pub const PROJECT_URI: &str = "project://current";

/// The server `instructions`: every tool, one line of when to use it.
pub const INSTRUCTIONS: &str = "kwikvid video editor. Tools: \
    project_state (read the project), \
    media_add / media_remove (library), \
    edit_pin_range / edit_unpin_range (force a moment into the edit, or stop forcing it), \
    edit_exclude_range / edit_unexclude_range (keep a moment out, or allow it again), \
    edit_undo / edit_redo. \
    Write tools rebuild the auto-edit when the project has music and its analysis, \
    and return revision, can_undo, can_redo, dirty, manual_edits_replaced, \
    regen_error and an edit summary. The project://current resource holds the \
    same JSON as project_state. All times are integer microseconds.";

/// The MCP server: one session (project, undo history, media root) and the
/// backends its tools use.
#[derive(Clone)]
pub struct KwikvidServer {
    session: Arc<Mutex<Session>>,
    ctx: JobContext,
    tool_router: ToolRouter<Self>,
}

impl std::fmt::Debug for KwikvidServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KwikvidServer")
            .field("media_root", &self.lock().media_root())
            .finish_non_exhaustive()
    }
}

impl KwikvidServer {
    /// A server holding a new, empty project. `root` (absolute) is the
    /// directory media paths are relative to; the server never reads or
    /// writes outside it. `ctx` supplies the media backend (probing), the
    /// music analyzer and the cache; tests inject fakes.
    pub fn new(root: PathBuf, ctx: JobContext) -> Self {
        KwikvidServer {
            session: Arc::new(Mutex::new(Session::new(root))),
            ctx,
            tool_router: Self::project_router() + Self::media_router() + Self::edit_router(),
        }
    }

    /// A copy of the current project.
    pub fn project(&self) -> Project {
        self.lock().project().clone()
    }

    /// The current session state, as `project_state` returns it.
    pub fn snapshot(&self) -> Snapshot {
        self.lock().snapshot()
    }

    /// The backends the tools use.
    pub fn context(&self) -> &JobContext {
        &self.ctx
    }

    /// The session. Do not hold the guard across a long operation (probe,
    /// analysis): every tool call waits for it.
    pub(crate) fn lock(&self) -> MutexGuard<'_, Session> {
        // A panic while holding the lock cannot leave the session
        // half-updated (History swaps in fully built projects), so poisoning
        // is harmless.
        self.session.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The media root (`--root`).
    pub(crate) fn root(&self) -> PathBuf {
        self.lock().media_root().to_owned()
    }

    /// Apply `cmd` through [`Session::apply_and_regenerate`] and report the
    /// result (or the rejection, as an error result).
    pub(crate) fn apply(&self, cmd: Command) -> Result<CallToolResult, ErrorData> {
        tracing::debug!(?cmd, "applying command");
        let mut session = self.lock();
        let analyses = session.analyses();
        match session.apply_and_regenerate(cmd, &analyses) {
            Ok(r) => tools::json_result(&WriteResult::from_regen(&r, None)),
            Err(e) => Ok(tools::tool_error(e.to_string())),
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
        .with_instructions(INSTRUCTIONS)
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
        .with_description("The kwikvid session snapshot (project, undo state) as JSON")
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
        let contents = ResourceContents::text(tools::snapshot_json(self)?, request.uri)
            .with_mime_type("application/json");
        Ok(ReadResourceResult::new(vec![contents]).into())
    }
}

/// A [`MediaBackend`] for when FFmpeg could not be loaded: every call fails
/// with a message saying so, so the server still starts and only the tools
/// that need media access report the problem.
#[derive(Debug, Clone)]
pub struct NoMediaBackend {
    reason: String,
}

impl NoMediaBackend {
    /// `reason`: why the real backend is unavailable (its init error).
    pub fn new(reason: impl Into<String>) -> Self {
        NoMediaBackend {
            reason: reason.into(),
        }
    }

    fn error(&self) -> MediaError {
        MediaError::Unsupported(format!("FFmpeg is unavailable ({})", self.reason))
    }
}

impl MediaBackend for NoMediaBackend {
    fn probe(&self, _path: &Path) -> kv_media::Result<MediaInfo> {
        Err(self.error())
    }

    fn open_video(
        &self,
        _path: &Path,
        _opts: &kv_media::DecodeOptions,
    ) -> kv_media::Result<Box<dyn kv_media::VideoReader>> {
        Err(self.error())
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;
    use kv_audio::MusicAnalysis;
    use kv_core::Micros;
    use kv_media::{MediaError, MediaInfo};
    use kv_session::MusicAnalyzer;
    use std::error::Error;

    pub const SEC: i64 = 1_000_000;

    /// Probes every file as 7 s long and records the path it was asked for.
    #[derive(Default)]
    pub struct FakeBackend(pub Mutex<Vec<PathBuf>>);

    impl MediaBackend for FakeBackend {
        fn probe(&self, path: &Path) -> kv_media::Result<MediaInfo> {
            self.0.lock().unwrap().push(path.to_owned());
            if path.ends_with("missing.mp4") {
                return Err(MediaError::NoStreams(path.to_owned()));
            }
            Ok(MediaInfo {
                duration: Micros(7 * SEC),
                container: "fake".into(),
                video: None,
                audio: None,
                creation_time: None,
            })
        }

        fn open_video(
            &self,
            path: &Path,
            _opts: &kv_media::DecodeOptions,
        ) -> kv_media::Result<Box<dyn kv_media::VideoReader>> {
            Err(MediaError::NoVideoStream(path.to_owned()))
        }
    }

    /// Never called by the tools tested here (no jobs run).
    pub struct FakeAnalyzer;

    impl MusicAnalyzer for FakeAnalyzer {
        fn analyze(&self, _path: &Path) -> Result<MusicAnalysis, Box<dyn Error + Send + Sync>> {
            Err("fake analyzer".into())
        }
    }

    /// 120 bpm, `secs` long.
    pub fn music(secs: i64) -> MusicAnalysis {
        let beats: Vec<Micros> = (0..secs * 2).map(|i| Micros(i * SEC / 2)).collect();
        MusicAnalysis {
            duration: Micros(secs * SEC),
            tempo_bpm: 120.0,
            downbeats: beats.iter().step_by(4).copied().collect(),
            beats,
            onsets: Vec::new(),
            energy: vec![0.3; (secs * 10) as usize],
        }
    }

    pub fn root() -> PathBuf {
        std::env::temp_dir().join("kv-mcp-root")
    }

    pub fn ctx(media: Arc<dyn MediaBackend>) -> JobContext {
        JobContext::new(
            media,
            Arc::new(FakeAnalyzer),
            Arc::new(kv_cache::Cache::in_memory().unwrap()),
        )
    }

    pub fn server_with(media: Arc<dyn MediaBackend>) -> KwikvidServer {
        KwikvidServer::new(root(), ctx(media))
    }

    pub fn server() -> KwikvidServer {
        server_with(Arc::new(FakeBackend::default()))
    }

    pub fn text(r: &CallToolResult) -> String {
        r.content[0].as_text().unwrap().text.clone()
    }

    /// The JSON of a successful result.
    pub fn ok(r: Result<CallToolResult, ErrorData>) -> serde_json::Value {
        let r = r.unwrap();
        assert_ne!(r.is_error, Some(true), "{}", text(&r));
        serde_json::from_str(&text(&r)).unwrap()
    }

    /// The message of an error result.
    pub fn err(r: Result<CallToolResult, ErrorData>) -> String {
        let r = r.unwrap();
        assert_eq!(r.is_error, Some(true), "expected an error: {}", text(&r));
        text(&r)
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::*;
    use super::*;

    #[test]
    fn instructions_and_readme_list_every_tool() {
        let server = server();
        let readme = include_str!("../README.md");
        let tools = server.tool_router.list_all();
        assert_eq!(tools.len(), 9, "{tools:?}");
        for tool in tools {
            assert!(INSTRUCTIONS.contains(&*tool.name), "{} missing", tool.name);
            assert!(
                readme.contains(&format!("| `{}` |", tool.name)),
                "{} missing from the README tool table",
                tool.name
            );
        }
    }

    #[test]
    fn no_media_backend_says_ffmpeg_is_unavailable() {
        let backend = NoMediaBackend::new("libavformat not found");
        let e = backend.probe(Path::new("/x.mp4")).unwrap_err().to_string();
        assert!(e.contains("FFmpeg is unavailable"), "{e}");
        assert!(e.contains("libavformat not found"), "{e}");
    }
}
