//! Media library tools: `media_add`, `media_import`, `media_remove`.

use kv_core::{Command, MediaId, MediaItem, MediaKind, Micros};
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::{schemars, tool, tool_router, ErrorData};
use serde::{Deserialize, Serialize};

use super::{json_result, relative_path, tool_error, WriteResult};
use crate::KwikvidServer;

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

/// Input of `media_remove`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct MediaRemoveParams {
    /// Id of the media item (see `media_add` or `project_state`).
    pub media_id: u64,
}

/// Input of `media_import`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct MediaImportParams {
    /// Files and folders to import: relative to the project root (the
    /// server's `--root`), or absolute inside it. Folders are scanned (not
    /// recursively) for MP4 and MOV files.
    pub paths: Vec<String>,
}

/// A file `media_import` did not add.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SkippedFile {
    /// The file, relative to the root when it is inside it.
    pub path: String,
    /// Why it was skipped (already in the library, no video, ...).
    pub reason: String,
}

/// What `media_import` returns: the import report plus the write result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ImportResult {
    /// Ids of the new library items, in import order (one per recording:
    /// GoPro chapter files are grouped).
    pub added: Vec<u64>,
    pub skipped: Vec<SkippedFile>,
    #[serde(flatten)]
    pub write: WriteResult,
}

impl KwikvidServer {
    pub(crate) fn media_import_impl(
        &self,
        p: MediaImportParams,
    ) -> Result<CallToolResult, ErrorData> {
        if p.paths.is_empty() {
            return Ok(tool_error("give at least one file or folder in paths"));
        }
        let root = self.root();
        let mut abs = Vec::with_capacity(p.paths.len());
        for path in &p.paths {
            match relative_path(&root, path) {
                Ok(rel) => abs.push(root.join(rel)),
                Err(msg) => return Ok(tool_error(msg)),
            }
        }
        // Probing and HiLights are cached and only read headers; the
        // session lock is held, as `Session::import` needs it throughout.
        let mut session = self.lock();
        match session.import(&abs, &self.ctx) {
            Ok((report, regen)) => json_result(&ImportResult {
                added: report.added.iter().map(|id| id.0).collect(),
                skipped: report
                    .skipped
                    .into_iter()
                    .map(|(path, reason)| SkippedFile {
                        path: path
                            .strip_prefix(&root)
                            .unwrap_or(&path)
                            .to_string_lossy()
                            .into_owned(),
                        reason,
                    })
                    .collect(),
                write: WriteResult::from_regen(&regen, None),
            }),
            Err(e) => Ok(tool_error(e.to_string())),
        }
    }

    pub(crate) fn media_add_impl(&self, p: MediaAddParams) -> Result<CallToolResult, ErrorData> {
        let path = match self.project_path(&p.path) {
            Ok(path) => path,
            Err(msg) => return Ok(tool_error(msg)),
        };
        let duration = match p.duration_us {
            Some(us) => Micros(us),
            None => {
                // Probe without holding the session lock.
                let file = self.lock().resolve(&path);
                match self.ctx.media.probe(&file) {
                    Ok(info) => info.duration,
                    Err(e) => {
                        return Ok(tool_error(format!(
                            "cannot probe {path:?}: {e}; pass duration_us to add it without probing"
                        )))
                    }
                }
            }
        };
        let mut session = self.lock();
        let id = session.project().next_media_id();
        let item = MediaItem {
            id,
            path,
            duration,
            kind: p.kind.into(),
            hilights: p.hilights_us.into_iter().map(Micros).collect(),
            clip_audio: false,
            chapters: Vec::new(),
        };
        tracing::debug!(?item, "adding media");
        let analyses = session.analyses();
        match session.apply_and_regenerate(Command::AddMedia { item }, &analyses) {
            Ok(r) => json_result(&WriteResult::from_regen(&r, Some(id.0))),
            Err(e) => Ok(tool_error(e.to_string())),
        }
    }
}

#[tool_router(router = media_router, vis = "pub(crate)")]
impl KwikvidServer {
    #[tool(
        description = "Add a clip or photo to the project library (kv-core AddMedia) and \
        rebuild the edit. Returns the write result with the new media_id."
    )]
    fn media_add(
        &self,
        Parameters(p): Parameters<MediaAddParams>,
    ) -> Result<CallToolResult, ErrorData> {
        self.media_add_impl(p)
    }

    #[tool(
        description = "Import video files and folders (MP4/MOV; folders not recursively) \
        into the library as one undo step: each file is probed and its HiLights read \
        (cached), and chaptered GoPro recordings (GX01xxxx, GX02xxxx, ...) become one item. \
        Returns added (new media ids), skipped (path and reason) and the write result. \
        Like media_add, it rebuilds the edit in the same undo step."
    )]
    fn media_import(
        &self,
        Parameters(p): Parameters<MediaImportParams>,
    ) -> Result<CallToolResult, ErrorData> {
        self.media_import_impl(p)
    }

    #[tool(
        description = "Remove a clip or photo from the project, with its pins, excludes \
        and cuts (kv-core RemoveMedia), and rebuild the edit."
    )]
    fn media_remove(
        &self,
        Parameters(p): Parameters<MediaRemoveParams>,
    ) -> Result<CallToolResult, ErrorData> {
        self.apply(Command::RemoveMedia {
            media_id: MediaId(p.media_id),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::*;
    use crate::{NoMediaBackend, RangeParams};
    use std::sync::Arc;

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
        let backend = Arc::new(FakeBackend::default());
        let server = server_with(backend.clone());
        let r = ok(server.media_add_impl(add("clips/a.mp4", None)));
        assert_eq!(r["media_id"], 1);
        assert_eq!(r["revision"], 1);
        assert_eq!(r["can_undo"], true);
        assert_eq!(*backend.0.lock().unwrap(), [root().join("clips/a.mp4")]);
        let p = server.project();
        assert_eq!(p.media[0].duration, Micros(7 * SEC));
        assert_eq!(p.media[0].path, "clips/a.mp4");
    }

    #[test]
    fn media_add_reports_probe_failure_and_missing_ffmpeg() {
        let server = server();
        let msg = err(server.media_add_impl(add("missing.mp4", None)));
        assert!(msg.contains("duration_us"), "{msg}");
        assert!(server.project().media.is_empty());

        let server = server_with(Arc::new(NoMediaBackend::new("not installed")));
        let msg = err(server.media_add_impl(add("a.mp4", None)));
        assert!(msg.contains("FFmpeg is unavailable"), "{msg}");
        assert!(msg.contains("duration_us"), "{msg}");
        // Media with a duration needs no FFmpeg.
        ok(server.media_add_impl(add("a.mp4", Some(3))));
        assert_eq!(server.project().media.len(), 1);
    }

    #[test]
    fn media_add_override_skips_probe() {
        let backend = Arc::new(FakeBackend::default());
        let server = server_with(backend.clone());
        ok(server.media_add_impl(add("a.mp4", Some(3))));
        assert!(backend.0.lock().unwrap().is_empty());
        assert_eq!(server.project().media[0].duration, Micros(3));
    }

    #[test]
    fn media_add_refuses_paths_outside_the_root() {
        let server = server();
        let outside = std::env::temp_dir().join("elsewhere.mp4");
        let msg = err(server.media_add_impl(add(outside.to_str().unwrap(), Some(3))));
        assert!(msg.contains("outside the project root"), "{msg}");
        err(server.media_add_impl(add("../up.mp4", Some(3))));
        assert!(server.project().media.is_empty());
    }

    #[test]
    fn media_remove_drops_its_pins() {
        let server = server();
        ok(server.media_add_impl(add("a.mp4", Some(10 * SEC))));
        ok(server.media_add_impl(add("b.mp4", Some(10 * SEC))));
        let pin = |media_id| RangeParams {
            media_id,
            start_us: SEC,
            end_us: 2 * SEC,
            section: None,
        };
        ok(server.edit_pin_range(Parameters(pin(1))));
        ok(server.edit_pin_range(Parameters(pin(2))));
        let r = ok(server.media_remove(Parameters(MediaRemoveParams { media_id: 1 })));
        assert_eq!(r["can_undo"], true);
        assert!(r.get("media_id").is_none(), "{r}");
        let p = server.project();
        assert_eq!(p.media.len(), 1);
        let pins = &p.sections[0].pins;
        assert_eq!(pins.len(), 1);
        assert_eq!(pins[0].media_id, MediaId(2));

        let msg = err(server.media_remove(Parameters(MediaRemoveParams { media_id: 1 })));
        assert!(msg.contains("no longer in the project"), "{msg}");
    }

    /// Probes every file as a 4 s video.
    struct VideoBackend;

    impl kv_media::MediaBackend for VideoBackend {
        fn probe(&self, path: &std::path::Path) -> kv_media::Result<kv_media::MediaInfo> {
            if path.to_string_lossy().contains("bad") {
                return Err(kv_media::MediaError::NoStreams(path.to_owned()));
            }
            Ok(kv_media::MediaInfo {
                duration: Micros(4 * SEC),
                container: "fake".into(),
                video: Some(kv_media::VideoInfo {
                    codec: "fake".into(),
                    width: 320,
                    height: 240,
                    fps: kv_media::Rational::new(30, 1),
                    bit_depth: 8,
                    rotation: 0,
                }),
                audio: None,
                creation_time: None,
            })
        }

        fn open_video(
            &self,
            path: &std::path::Path,
            _opts: &kv_media::DecodeOptions,
        ) -> kv_media::Result<Box<dyn kv_media::VideoReader>> {
            Err(kv_media::MediaError::NoVideoStream(path.to_owned()))
        }
    }

    fn import(paths: &[&str]) -> MediaImportParams {
        MediaImportParams {
            paths: paths.iter().map(|p| (*p).to_owned()).collect(),
        }
    }

    #[test]
    fn media_import_groups_chapters_into_one_item() {
        let dir = tempfile::tempdir().unwrap();
        let clips = dir.path().join("clips");
        std::fs::create_dir(&clips).unwrap();
        for name in [
            "GX010001.MP4",
            "GX020001.MP4",
            "GX010001.LRV",
            "bad.mp4",
            "notes.txt",
        ] {
            std::fs::write(clips.join(name), name).unwrap();
        }
        let server = KwikvidServer::new(dir.path().to_owned(), ctx(Arc::new(VideoBackend)));

        let r = ok(server.media_import_impl(import(&["clips"])));
        assert_eq!(r["added"], serde_json::json!([1]), "{r}");
        let skipped = r["skipped"].as_array().unwrap();
        assert_eq!(skipped.len(), 1, "{r}");
        assert!(
            skipped[0]["path"].as_str().unwrap().ends_with("bad.mp4"),
            "{r}"
        );
        assert!(skipped[0]["reason"].is_string(), "{r}");
        assert_eq!(r["revision"], 1);
        assert_eq!(r["can_undo"], true);
        assert_eq!(r["edit"]["segments"], 0);
        // The import tried to rebuild the edit; with no music it cannot.
        assert_eq!(r["regen_error"], "Add music to create an edit.", "{r}");
        let p = server.project();
        assert_eq!(p.media.len(), 1);
        assert_eq!(p.media[0].path, "clips/GX010001.MP4");
        assert_eq!(p.media[0].chapters.len(), 2);
        assert_eq!(p.media[0].duration, Micros(8 * SEC));

        // Again: everything is already in the library; no undo step.
        let r = ok(server.media_import_impl(import(&["clips/GX020001.MP4"])));
        assert_eq!(r["added"], serde_json::json!([]));
        assert_eq!(r["skipped"][0]["reason"], "Already in the library.");
        assert_eq!(r["revision"], 1);

        // One undo step removes the whole import.
        server.lock().undo().unwrap();
        assert!(server.project().media.is_empty());
    }

    #[test]
    fn media_import_refuses_paths_outside_the_root() {
        let server = server_with(Arc::new(VideoBackend));
        let msg = err(server.media_import_impl(import(&["ok.mp4", "../up"])));
        assert!(msg.contains(".."), "{msg}");
        err(server.media_import_impl(import(&[])));
        assert_eq!(server.snapshot().revision, 0);
    }
}
