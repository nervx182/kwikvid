//! Export tools: `export_options`, `export_start`, `export_status`,
//! `export_cancel`.
//!
//! They mirror the desktop app's Export dialog (`crates/kv-app/src/export.rs`)
//! over [`kv_session::export`]: `export_start` takes the export input from
//! the session ([`Session::export_input`](kv_session::Session::export_input),
//! so later edits do not change a running export) and starts an
//! [`ExportJob`] with the server's media backend and its encoder
//! ([`Exports`]). The job reports [`ExportEvent`]s on its own thread; the
//! latest one is kept per export id behind a [`Condvar`], so
//! `export_status` can return at once or wait (`wait_ms`, at most
//! [`MAX_WAIT_MS`]) until the export is final. One export runs at a time.
//!
//! Exports are always 30 fps ([`kv_session::EXPORT_FPS`]). The file is
//! written as `.<name>.partial` and renamed when done, so a cancelled or
//! failed export leaves no file.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use kv_core::SectionId;
use kv_media::{Encoder, ExportSettings, MediaError, VideoCodec, VideoWriter};
use kv_session::{
    export_size, ExportEvent, ExportJob, ExportOptions, ExportRequest, ExportResolution,
    ExportUpdate, EXPORT_FPS,
};
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::{schemars, tool, tool_router, ErrorData};
use serde::{Deserialize, Serialize};

use super::edit::section_or_default;
use super::style::AspectParam;
use super::{json_result, relative_path, tool_error};
use crate::KwikvidServer;

/// The longest `export_status` waits (`wait_ms` is capped to it): 30 s.
pub const MAX_WAIT_MS: u64 = 30_000;

/// The encoder exports use, or why there is none (shown as `blocked`).
pub type EncoderBackend = Result<Arc<dyn Encoder>, String>;

/// The latest event of one export, and a condvar signalled on each new one.
#[derive(Default)]
struct Tracker {
    latest: Mutex<Option<ExportEvent>>,
    changed: Condvar,
}

impl Tracker {
    fn lock(&self) -> MutexGuard<'_, Option<ExportEvent>> {
        self.latest.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn set(&self, event: ExportEvent) {
        *self.lock() = Some(event);
        self.changed.notify_all();
    }

    /// The latest event, after waiting up to `wait` for a final one.
    fn wait_final(&self, wait: Duration) -> ExportEvent {
        let deadline = Instant::now() + wait;
        let mut latest = self.lock();
        loop {
            if latest.as_ref().is_some_and(ExportEvent::is_final) {
                break;
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            latest = self
                .changed
                .wait_timeout(latest, left)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
        latest.clone().unwrap_or(ExportEvent::Started)
    }
}

/// The server's exports: the encoder, the running job and the latest
/// event of every export started in this session.
pub struct Exports {
    encoder: EncoderBackend,
    state: Mutex<ExportState>,
    next_id: AtomicU64,
}

#[derive(Default)]
struct ExportState {
    running: Option<(u64, ExportJob)>,
    trackers: HashMap<u64, Arc<Tracker>>,
}

impl std::fmt::Debug for Exports {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Exports")
            .field("encoder", &self.encoder.as_ref().map(|_| "encoder"))
            .finish_non_exhaustive()
    }
}

impl Exports {
    pub fn new(encoder: EncoderBackend) -> Exports {
        Exports {
            encoder,
            state: Mutex::new(ExportState::default()),
            next_id: AtomicU64::new(1),
        }
    }

    fn lock(&self) -> MutexGuard<'_, ExportState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn tracker(&self, id: u64) -> Option<Arc<Tracker>> {
        self.lock().trackers.get(&id).cloned()
    }
}

/// An encoder that encodes nothing, for the options when there is none.
struct NoEncoder;

impl Encoder for NoEncoder {
    fn create(
        &self,
        _: &std::path::Path,
        _: &ExportSettings,
    ) -> kv_media::Result<Box<dyn VideoWriter>> {
        Err(MediaError::Unsupported("no encoder".into()))
    }

    fn supports(&self, _: VideoCodec) -> bool {
        false
    }
}

/// The video codec, as `export_options` lists it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CodecParam {
    /// H.264 / AVC: plays everywhere (the default).
    H264,
    /// H.265 / HEVC: smaller files; only where `export_options` lists it.
    Hevc,
}

impl From<CodecParam> for VideoCodec {
    fn from(c: CodecParam) -> Self {
        match c {
            CodecParam::H264 => VideoCodec::H264,
            CodecParam::Hevc => VideoCodec::Hevc,
        }
    }
}

fn codec_name(c: VideoCodec) -> &'static str {
    match c {
        VideoCodec::H264 => "h264",
        VideoCodec::Hevc => "hevc",
    }
}

/// The output resolution (short side of the frame).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub enum ResolutionParam {
    /// 1080 pixels: 1920x1080 at 16:9 (the default).
    #[serde(rename = "1080p")]
    Hd1080,
    /// 2160 pixels: 3840x2160 at 16:9.
    #[serde(rename = "4k")]
    Uhd4k,
}

impl From<ResolutionParam> for ExportResolution {
    fn from(r: ResolutionParam) -> Self {
        match r {
            ResolutionParam::Hd1080 => ExportResolution::Hd1080,
            ResolutionParam::Uhd4k => ExportResolution::Uhd4k,
        }
    }
}

/// Input of `export_options`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ExportOptionsParams {
    /// Section id; defaults to the project's only section (1).
    #[serde(default)]
    pub section: Option<u64>,
}

/// Input of `export_start`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ExportStartParams {
    /// The MP4 to write: relative to the project root (the server's
    /// `--root`) or absolute inside it, ending in `.mp4`. Its folder must
    /// exist. An existing file is replaced when the export is done.
    pub path: String,
    /// `h264` (default) or `hevc` (only where `export_options` lists it).
    #[serde(default)]
    pub codec: Option<CodecParam>,
    /// `1080p` (default) or `4k`.
    #[serde(default)]
    pub resolution: Option<ResolutionParam>,
    /// `landscape16x9`, `portrait9x16` or `square1x1`; defaults to the
    /// project's aspect ratio.
    #[serde(default)]
    pub aspect: Option<AspectParam>,
    /// Section id; defaults to the project's only section (1).
    #[serde(default)]
    pub section: Option<u64>,
}

/// Input of `export_status`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ExportStatusParams {
    /// The export id `export_start` returned.
    pub id: u64,
    /// Wait up to this many milliseconds (at most 30000) for the export to
    /// finish (done, failed or cancelled) before answering. Default 0:
    /// answer at once.
    #[serde(default)]
    #[schemars(range(min = 0, max = 30000))]
    pub wait_ms: Option<u64>,
}

/// Input of `export_cancel`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ExportCancelParams {
    /// The export id `export_start` returned.
    pub id: u64,
}

/// What `export_options` returns.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ExportOptionsResult {
    /// Codecs this machine can encode: `h264` first, `hevc` where available.
    pub codecs: Vec<&'static str>,
    pub aspect: kv_core::AspectRatio,
    pub resolution: ExportResolution,
    /// Suggested file name, `<project name>.mp4`.
    pub file_name: String,
    /// Why exporting is not possible now; `null` when it is.
    pub blocked: Option<String>,
}

impl From<ExportOptions> for ExportOptionsResult {
    fn from(o: ExportOptions) -> Self {
        ExportOptionsResult {
            codecs: o.codecs.into_iter().map(codec_name).collect(),
            aspect: o.aspect,
            resolution: o.resolution,
            file_name: o.file_name,
            blocked: o.blocked,
        }
    }
}

/// What `export_start` returns.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ExportStarted {
    /// Pass to `export_status` and `export_cancel`.
    pub id: u64,
    /// The destination, project-relative.
    pub path: String,
    pub codec: &'static str,
    pub resolution: ExportResolution,
    pub aspect: kv_core::AspectRatio,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
}

impl KwikvidServer {
    pub(crate) fn export_options_impl(&self, section: SectionId) -> ExportOptionsResult {
        let session = self.lock();
        match &self.exports.encoder {
            Ok(encoder) => session.export_options(section, &**encoder).into(),
            Err(why) => {
                let mut o = session.export_options(section, &NoEncoder);
                o.codecs = vec![VideoCodec::H264];
                o.blocked = Some(why.clone());
                o.into()
            }
        }
    }

    pub(crate) fn export_start_impl(
        &self,
        p: ExportStartParams,
    ) -> Result<CallToolResult, ErrorData> {
        let root = self.root();
        let rel = match relative_path(&root, &p.path) {
            Ok(rel) => rel,
            Err(msg) => return Ok(tool_error(msg)),
        };
        let is_mp4 = rel
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| e.eq_ignore_ascii_case("mp4"));
        if !is_mp4 {
            return Ok(tool_error(format!(
                "{:?} must end in .mp4 (exports are MPEG-4 video)",
                p.path
            )));
        }
        let encoder = match &self.exports.encoder {
            Ok(e) => Arc::clone(e),
            Err(why) => return Ok(tool_error(why.clone())),
        };
        let codec: VideoCodec = p.codec.unwrap_or(CodecParam::H264).into();
        if !encoder.supports(codec) {
            return Ok(tool_error(format!(
                "This machine can't encode {}; use h264 (see export_options).",
                codec_name(codec)
            )));
        }
        let section = section_or_default(p.section);
        let out: PathBuf = root.join(&rel);

        let mut state = self.exports.lock();
        if let Some((id, _)) = state.running.as_ref().filter(|(_, j)| !j.is_finished()) {
            return Ok(tool_error(format!(
                "Export {id} is still running; wait for it (export_status) or cancel it \
                 (export_cancel) first."
            )));
        }
        let (input, request) = {
            let session = self.lock();
            let request = ExportRequest {
                codec,
                resolution: p.resolution.unwrap_or(ResolutionParam::Hd1080).into(),
                aspect: p
                    .aspect
                    .map(Into::into)
                    .unwrap_or(session.project().settings.aspect_ratio),
                path: out,
            };
            match session.export_input(section, &request) {
                Ok(input) => (input, request),
                Err(why) => return Ok(tool_error(why)),
            }
        };
        let id = self.exports.next_id.fetch_add(1, Ordering::Relaxed);
        let tracker = Arc::new(Tracker::default());
        tracker.set(ExportEvent::Started);
        let events = Arc::clone(&tracker);
        let job = ExportJob::start(input, Arc::clone(&self.ctx.media), encoder, move |e| {
            events.set(e)
        });
        state.trackers.insert(id, tracker);
        state.running = Some((id, job));
        drop(state);
        tracing::info!(id, ?request, "export started");

        let (width, height) = export_size(request.aspect, request.resolution);
        json_result(&ExportStarted {
            id,
            path: project_relative(&rel),
            codec: codec_name(codec),
            resolution: request.resolution,
            aspect: request.aspect,
            width,
            height,
            fps: u32::try_from(EXPORT_FPS.num).unwrap_or(30),
        })
    }

    /// The latest event of export `id` as JSON (`{"id", "state", ...}`),
    /// waiting up to `wait_ms` for a final one. Blocks: call it off the
    /// async runtime.
    pub(crate) fn export_status_impl(
        &self,
        p: ExportStatusParams,
    ) -> Result<CallToolResult, ErrorData> {
        let Some(tracker) = self.exports.tracker(p.id) else {
            return Ok(tool_error(format!("No export with id {}.", p.id)));
        };
        let wait = Duration::from_millis(p.wait_ms.unwrap_or(0).min(MAX_WAIT_MS));
        let event = tracker.wait_final(wait);
        let mut value = serde_json::to_value(ExportUpdate { id: p.id, event })
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
        // The report's absolute path, project-relative like every path the
        // server returns.
        let root = self.root();
        if let Some(path) = value.pointer_mut("/report/path") {
            if let Some(rel) = path
                .as_str()
                .and_then(|s| std::path::Path::new(s).strip_prefix(&root).ok())
            {
                *path = serde_json::Value::String(project_relative(rel));
            }
        }
        json_result(&value)
    }

    pub(crate) fn export_cancel_impl(&self, id: u64) -> Result<CallToolResult, ErrorData> {
        let state = self.exports.lock();
        if !state.trackers.contains_key(&id) {
            return Ok(tool_error(format!("No export with id {id}.")));
        }
        match &state.running {
            Some((running, job)) if *running == id && !job.is_finished() => {
                job.cancel();
                tracing::info!(id, "export cancel requested");
                json_result(&serde_json::json!({"id": id, "cancel_requested": true}))
            }
            _ => Ok(tool_error(format!(
                "Export {id} has already finished; see export_status."
            ))),
        }
    }
}

/// `rel` as a `/`-separated string.
fn project_relative(rel: &std::path::Path) -> String {
    rel.components()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

#[tool_router(router = export_router, vis = "pub(crate)")]
impl KwikvidServer {
    #[tool(
        description = "What an export of a section can use: codecs this machine can \
        encode (h264, maybe hevc), the project's aspect, the default resolution (1080p), \
        a suggested file_name, and blocked (why exporting is not possible now, or null)."
    )]
    fn export_options(
        &self,
        Parameters(p): Parameters<ExportOptionsParams>,
    ) -> Result<CallToolResult, ErrorData> {
        json_result(&self.export_options_impl(section_or_default(p.section)))
    }

    #[tool(
        description = "Start exporting a section's edit to an MP4 inside the project root \
        (30 fps; defaults: h264, 1080p, the project's aspect). Returns at once with an \
        export id, size and fps; poll export_status (with wait_ms) until done. One export \
        at a time; refused when export_options reports blocked."
    )]
    fn export_start(
        &self,
        Parameters(p): Parameters<ExportStartParams>,
    ) -> Result<CallToolResult, ErrorData> {
        self.export_start_impl(p)
    }

    #[tool(
        description = "The latest state of an export: {id, state} where state is started, \
        progress (frame, total_frames, fraction, eta_ms), done (report: path, frames, \
        duration, encoder_name, elapsed_ms), failed (message) or cancelled. wait_ms (max \
        30000) waits for the export to finish before answering."
    )]
    async fn export_status(
        &self,
        Parameters(p): Parameters<ExportStatusParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let server = self.clone();
        tokio::task::spawn_blocking(move || server.export_status_impl(p))
            .await
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))?
    }

    #[tool(
        description = "Cancel a running export; export_status then reaches cancelled and \
        no file is left. An error when the export has already finished."
    )]
    fn export_cancel(
        &self,
        Parameters(p): Parameters<ExportCancelParams>,
    ) -> Result<CallToolResult, ErrorData> {
        self.export_cancel_impl(p.id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::*;

    /// Claims to encode H.264 only; never asked to encode here.
    struct FakeEncoder;

    impl Encoder for FakeEncoder {
        fn create(
            &self,
            _: &std::path::Path,
            _: &ExportSettings,
        ) -> kv_media::Result<Box<dyn VideoWriter>> {
            Err(MediaError::Unsupported("fake".into()))
        }

        fn supports(&self, c: VideoCodec) -> bool {
            c == VideoCodec::H264
        }
    }

    fn with_encoder() -> KwikvidServer {
        server().with_encoder(Ok(Arc::new(FakeEncoder)))
    }

    fn start(path: &str) -> ExportStartParams {
        ExportStartParams {
            path: path.into(),
            codec: None,
            resolution: None,
            aspect: None,
            section: None,
        }
    }

    const EMPTY: &str = "Nothing to export yet: add clips and music to make an edit.";

    #[test]
    fn options_list_codecs_and_blocked_reason() {
        let o = with_encoder().export_options_impl(SectionId(1));
        assert_eq!(o.codecs, ["h264"]);
        assert_eq!(o.file_name, "Untitled.mp4");
        assert_eq!(o.blocked.as_deref(), Some(EMPTY));
        let v = serde_json::to_value(&o).unwrap();
        assert_eq!(v["aspect"], "landscape16x9");
        assert_eq!(v["resolution"], "1080p");

        // No encoder: blocked says why.
        let o = server()
            .with_encoder(Err("Export is unavailable: no FFmpeg".into()))
            .export_options_impl(SectionId(1));
        assert_eq!(
            o.blocked.as_deref(),
            Some("Export is unavailable: no FFmpeg")
        );
    }

    #[test]
    fn start_on_an_empty_edit_is_refused_with_the_blocked_message() {
        let server = with_encoder();
        assert_eq!(err(server.export_start_impl(start("out.mp4"))), EMPTY);
        let mut hevc = start("out.mp4");
        hevc.codec = Some(CodecParam::Hevc);
        assert!(err(server.export_start_impl(hevc)).contains("can't encode hevc"));
    }

    #[test]
    fn start_refuses_paths_outside_the_root_and_non_mp4() {
        let server = with_encoder();
        assert!(err(server.export_start_impl(start("../out.mp4"))).contains(".."));
        let outside = std::env::temp_dir().join("elsewhere.mp4");
        assert!(
            err(server.export_start_impl(start(outside.to_str().unwrap())))
                .contains("outside the project root")
        );
        assert!(err(server.export_start_impl(start("out.mov"))).contains(".mp4"));
    }

    #[test]
    fn status_and_cancel_of_an_unknown_export_are_errors() {
        let server = with_encoder();
        let r = server.export_status_impl(ExportStatusParams {
            id: 7,
            wait_ms: None,
        });
        assert_eq!(err(r), "No export with id 7.");
        assert_eq!(err(server.export_cancel_impl(7)), "No export with id 7.");
    }

    #[test]
    fn tracker_waits_for_a_final_event_or_times_out() {
        let t = Arc::new(Tracker::default());
        t.set(ExportEvent::Started);
        let begun = Instant::now();
        assert_eq!(
            t.wait_final(Duration::from_millis(50)),
            ExportEvent::Started
        );
        assert!(begun.elapsed() >= Duration::from_millis(50));
        let t2 = Arc::clone(&t);
        let h = std::thread::spawn(move || t2.wait_final(Duration::from_secs(10)));
        std::thread::sleep(Duration::from_millis(20));
        t.set(ExportEvent::Cancelled);
        assert_eq!(h.join().unwrap(), ExportEvent::Cancelled);
    }

    #[test]
    fn codec_and_resolution_params_use_the_option_names() {
        let p: ExportStartParams = serde_json::from_value(serde_json::json!({
            "path": "a.mp4", "codec": "hevc", "resolution": "4k", "aspect": "square1x1"
        }))
        .unwrap();
        assert_eq!(p.codec, Some(CodecParam::Hevc));
        assert_eq!(p.resolution, Some(ResolutionParam::Uhd4k));
        assert_eq!(p.aspect, Some(AspectParam::Square1x1));
    }
}
