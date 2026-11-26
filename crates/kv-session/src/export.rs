//! Export: what the Export dialog offers, and the export job.
//!
//! - [`export_size`] maps an [`AspectRatio`] and an [`ExportResolution`] to
//!   the output size; the frame rate is always [`EXPORT_FPS`].
//! - [`Session::export_options`] is what the dialog opens with: the codecs
//!   this system can encode, the project's aspect ratio, a file name and,
//!   when exporting is not possible yet, the reason
//!   ([`Session::export_blocker`]).
//! - [`ExportJob::start`] runs [`kv_render::export`] on its own thread (an
//!   export takes minutes, so it does not occupy a [`JobRunner`] worker
//!   that probes and analyses) and reports [`ExportEvent`]s; it can be
//!   cancelled. The file is written next to the destination as
//!   `.<name>.partial` and renamed when complete, so a cancelled or failed
//!   export leaves no file and never clobbers an existing one.
//!
//! # Frame rate
//!
//! Exports are 30 fps ([`EXPORT_FPS`]), whatever the sources. GoPro footage
//! is mostly 30/60 fps (or 24/25, which 30 handles with repeated frames),
//! the edit is cut to music beats rather than source frames, and 30 fps is
//! what phones and social apps play everywhere. Following the dominant
//! source rate (rounded to 30 or 60) is a possible later setting.
//!
//! [`JobRunner`]: crate::JobRunner

use std::ops::ControlFlow;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use kv_core::{AspectRatio, Micros, Project, SectionId};
use kv_media::{Encoder, ExportSettings, MediaBackend, Rational, VideoCodec};
use kv_render::{ExportReport, Progress, RenderError};
use serde::{Deserialize, Serialize};

use crate::{MissingKind, Session};

/// Frame rate of every export (see the module docs).
pub const EXPORT_FPS: Rational = Rational::new(30, 1);

/// Output resolution: the short side of the frame is 1080 or 2160 pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum ExportResolution {
    /// Full HD: 1920x1080 at 16:9.
    #[default]
    #[serde(rename = "1080p")]
    Hd1080,
    /// 4K UHD: 3840x2160 at 16:9.
    #[serde(rename = "4k")]
    Uhd4k,
}

/// Output width and height for `aspect` at `resolution`:
///
/// | | 16:9 | 9:16 | 1:1 |
/// | --- | --- | --- | --- |
/// | 1080p | 1920x1080 | 1080x1920 | 1080x1080 |
/// | 4K | 3840x2160 | 2160x3840 | 2160x2160 |
pub fn export_size(aspect: AspectRatio, resolution: ExportResolution) -> (u32, u32) {
    let (long, short) = match resolution {
        ExportResolution::Hd1080 => (1920, 1080),
        ExportResolution::Uhd4k => (3840, 2160),
    };
    match aspect {
        AspectRatio::Landscape16x9 => (long, short),
        AspectRatio::Portrait9x16 => (short, long),
        AspectRatio::Square1x1 => (short, short),
    }
}

/// What the Export dialog asks for. JSON:
/// `{"codec":"H264","resolution":"1080p","aspect":"portrait9x16","path":"/Users/ana/Movies/Trip.mp4"}`
/// (`codec` is `"H264"` or `"hevc"`, `resolution` `"1080p"` or `"4k"`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExportRequest {
    pub codec: VideoCodec,
    pub resolution: ExportResolution,
    pub aspect: AspectRatio,
    /// The destination file (absolute).
    pub path: PathBuf,
}

impl ExportRequest {
    /// The encoder settings: [`export_size`], [`EXPORT_FPS`], the codec's
    /// [`ExportSettings::default_bitrate`], 48 kHz stereo.
    pub fn settings(&self) -> ExportSettings {
        let (width, height) = export_size(self.aspect, self.resolution);
        ExportSettings {
            width,
            height,
            fps: EXPORT_FPS,
            video_codec: self.codec,
            video_bitrate: ExportSettings::default_bitrate(width, height, self.codec),
            ..ExportSettings::default()
        }
    }
}

/// What the Export dialog opens with.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExportOptions {
    /// The codecs this system can encode, H.264 first (HEVC only where
    /// [`Encoder::supports`] says so).
    pub codecs: Vec<VideoCodec>,
    /// The project's aspect ratio (the dialog's initial choice).
    pub aspect: AspectRatio,
    /// The initial resolution.
    pub resolution: ExportResolution,
    /// The suggested file name: `<project name>.mp4`.
    pub file_name: String,
    /// Why exporting is not possible right now; `None` when it is.
    pub blocked: Option<String>,
}

impl Session {
    /// The project's name: its file name without the extension, or
    /// `"Untitled"`.
    pub fn project_name(&self) -> String {
        self.file_path()
            .and_then(|f| f.file_stem())
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "Untitled".into())
    }

    /// Why `section` cannot be exported now, as a sentence for the UI: it
    /// does not exist, its edit is empty, or a clip it uses (or its music)
    /// is missing on disk. `None` when it can be exported.
    pub fn export_blocker(&self, section: SectionId) -> Option<String> {
        let project = self.project();
        let Some(sec) = project.section(section) else {
            return Some("That part of the project no longer exists.".into());
        };
        if sec.edit.segments.is_empty() {
            return Some("Nothing to export yet: add clips and music to make an edit.".into());
        }
        let missing = self.missing_files();
        let clips = missing
            .iter()
            .filter(|m| match m.kind {
                MissingKind::Media { media_id } => {
                    sec.edit.segments.iter().any(|s| s.media_id == media_id)
                }
                MissingKind::Music { .. } => false,
            })
            .count();
        let music = missing
            .iter()
            .any(|m| m.kind == MissingKind::Music { section });
        match (clips, music) {
            (0, false) => None,
            (0, true) => Some("The music file is missing. Choose the song again to export.".into()),
            (1, _) => Some("A clip in the edit is missing. Relink or remove it to export.".into()),
            (n, _) => Some(format!(
                "{n} clips in the edit are missing. Relink or remove them to export."
            )),
        }
    }

    /// The Export dialog's initial state for `section`, with the codecs
    /// `encoder` supports.
    pub fn export_options(&self, section: SectionId, encoder: &dyn Encoder) -> ExportOptions {
        let codecs = [VideoCodec::H264, VideoCodec::Hevc]
            .into_iter()
            .filter(|&c| encoder.supports(c))
            .collect();
        ExportOptions {
            codecs,
            aspect: self.project().settings.aspect_ratio,
            resolution: ExportResolution::default(),
            file_name: format!("{}.mp4", self.project_name()),
            blocked: self.export_blocker(section),
        }
    }
}

/// A finished export, as the UI shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExportSummary {
    /// The file written.
    pub path: PathBuf,
    /// Video frames written.
    pub frames: u64,
    /// Length of the video.
    pub duration: Micros,
    /// Video encoder used, e.g. `"h264_videotoolbox"`.
    pub encoder_name: String,
    /// How long the export took, in milliseconds.
    pub elapsed_ms: u64,
}

impl ExportSummary {
    fn new(path: PathBuf, r: &ExportReport) -> ExportSummary {
        ExportSummary {
            path,
            frames: r.frames,
            duration: r.duration,
            encoder_name: r.encoder_name.clone(),
            elapsed_ms: millis(r.elapsed),
        }
    }
}

/// What an [`ExportJob`] reports, in order: `Started`, `Progress`
/// (repeatedly), then one of `Done`, `Failed` or `Cancelled`.
///
/// JSON is tagged by `state`, e.g.
/// `{"state":"progress","frame":30,"total_frames":300,"fraction":0.1,"eta_ms":9000}`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ExportEvent {
    Started,
    /// Sent whenever the whole percentage changes, and for the last frame.
    Progress {
        frame: u64,
        total_frames: u64,
        /// `frame / total_frames`.
        fraction: f32,
        /// Estimated time left in milliseconds ([`eta`]).
        eta_ms: Option<u64>,
    },
    Done {
        report: ExportSummary,
    },
    /// `message` is a short, readable sentence.
    Failed {
        message: String,
    },
    Cancelled,
}

impl ExportEvent {
    /// Whether this is the job's last event.
    pub fn is_final(&self) -> bool {
        matches!(
            self,
            ExportEvent::Done { .. } | ExportEvent::Failed { .. } | ExportEvent::Cancelled
        )
    }
}

/// Time left, assuming the remaining frames take as long as the done ones
/// on average; `None` before the first frame.
pub fn eta(elapsed: Duration, frame: u64, total_frames: u64) -> Option<Duration> {
    if frame == 0 {
        return None;
    }
    let left = total_frames.saturating_sub(frame);
    let nanos = elapsed.as_nanos() * u128::from(left) / u128::from(frame);
    Some(Duration::from_nanos(
        u64::try_from(nanos).unwrap_or(u64::MAX),
    ))
}

fn millis(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

/// The hidden file an export writes before it is renamed to `out`:
/// `.<name>.partial` in the same folder.
pub fn partial_path(out: &Path) -> PathBuf {
    let mut name = std::ffi::OsString::from(".");
    name.push(out.file_name().unwrap_or_default());
    name.push(".partial");
    out.with_file_name(name)
}

/// Everything an export needs, taken from the session when it starts (so
/// edits made meanwhile do not change it).
pub struct ExportInput {
    pub project: Project,
    pub section: SectionId,
    /// The directory the project's media paths are relative to.
    pub media_root: PathBuf,
    /// The destination file.
    pub out: PathBuf,
    pub settings: ExportSettings,
}

impl Session {
    /// The input of an export of `section` with `request`, or why it cannot
    /// run ([`Session::export_blocker`]).
    pub fn export_input(
        &self,
        section: SectionId,
        request: &ExportRequest,
    ) -> Result<ExportInput, String> {
        if let Some(why) = self.export_blocker(section) {
            return Err(why);
        }
        Ok(ExportInput {
            project: self.project().clone(),
            section,
            media_root: self.media_root().to_path_buf(),
            out: request.path.clone(),
            settings: request.settings(),
        })
    }
}

/// One export running on its own thread.
///
/// Dropping the handle does not stop the export; call
/// [`ExportJob::cancel`] first.
pub struct ExportJob {
    cancel: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl std::fmt::Debug for ExportJob {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExportJob")
            .field("cancelled", &self.cancel.load(Ordering::SeqCst))
            .finish_non_exhaustive()
    }
}

impl ExportJob {
    /// Start exporting `input` with `media` and `encoder`; every
    /// [`ExportEvent`] goes to `on_event`, on the export thread.
    pub fn start(
        input: ExportInput,
        media: Arc<dyn MediaBackend>,
        encoder: Arc<dyn Encoder>,
        mut on_event: impl FnMut(ExportEvent) + Send + 'static,
    ) -> ExportJob {
        let cancel = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&cancel);
        let thread = thread::Builder::new()
            .name("kv-export".into())
            .spawn(move || {
                on_event(ExportEvent::Started);
                let event = run(&input, &*media, &*encoder, &flag, &mut on_event);
                on_event(event);
            })
            .expect("failed to spawn the export thread");
        ExportJob {
            cancel,
            thread: Some(thread),
        }
    }

    /// Ask the export to stop after the current frame; it then reports
    /// `Cancelled` and removes its partial file.
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::SeqCst);
    }

    /// Whether the export thread has sent its final event and exited.
    pub fn is_finished(&self) -> bool {
        self.thread.as_ref().is_none_or(|t| t.is_finished())
    }

    /// Block until the export thread exits.
    pub fn wait(mut self) {
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// The export itself; returns the final event.
fn run(
    input: &ExportInput,
    media: &dyn MediaBackend,
    encoder: &dyn Encoder,
    cancel: &AtomicBool,
    on_event: &mut dyn FnMut(ExportEvent),
) -> ExportEvent {
    let out = &input.out;
    let tmp = partial_path(out);
    let started = Instant::now();
    let mut last_percent = None;
    let mut progress = |p: Progress| {
        if cancel.load(Ordering::SeqCst) {
            return ControlFlow::Break(());
        }
        let percent = p.frame * 100 / p.total_frames.max(1);
        if last_percent != Some(percent) || p.frame == p.total_frames {
            last_percent = Some(percent);
            on_event(ExportEvent::Progress {
                frame: p.frame,
                total_frames: p.total_frames,
                fraction: p.frame as f32 / p.total_frames.max(1) as f32,
                eta_ms: eta(started.elapsed(), p.frame, p.total_frames).map(millis),
            });
        }
        ControlFlow::Continue(())
    };
    tracing::info!(out = %out.display(), settings = ?input.settings, "export started");
    let result = kv_render::export(
        &input.project,
        input.section,
        &input.media_root,
        media,
        encoder,
        &tmp,
        &input.settings,
        &mut progress,
    );
    let event = match result {
        Ok(report) => match std::fs::rename(&tmp, out) {
            Ok(()) => ExportEvent::Done {
                report: ExportSummary::new(out.clone(), &report),
            },
            Err(e) => {
                let _ = std::fs::remove_file(&tmp);
                ExportEvent::Failed {
                    message: format!("Couldn't write \"{}\": {e}", label(out)),
                }
            }
        },
        Err(RenderError::Cancelled) => ExportEvent::Cancelled,
        Err(e) => {
            // kv-render removes its partial file; make sure.
            let _ = std::fs::remove_file(&tmp);
            ExportEvent::Failed {
                message: format!("The export failed: {e}"),
            }
        }
    };
    tracing::info!(?event, "export finished");
    event
}

fn label(path: &Path) -> String {
    path.file_name()
        .unwrap_or(path.as_os_str())
        .to_string_lossy()
        .into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use kv_core::{Command, MediaId, MediaItem, MediaKind, Segment, Speed, Transition};

    #[test]
    fn export_sizes() {
        use AspectRatio::*;
        use ExportResolution::*;
        assert_eq!(export_size(Landscape16x9, Hd1080), (1920, 1080));
        assert_eq!(export_size(Portrait9x16, Hd1080), (1080, 1920));
        assert_eq!(export_size(Square1x1, Hd1080), (1080, 1080));
        assert_eq!(export_size(Landscape16x9, Uhd4k), (3840, 2160));
        assert_eq!(export_size(Portrait9x16, Uhd4k), (2160, 3840));
        assert_eq!(export_size(Square1x1, Uhd4k), (2160, 2160));
    }

    #[test]
    fn request_settings_and_json() {
        let req: ExportRequest = serde_json::from_value(serde_json::json!({
            "codec": "hevc", "resolution": "4k", "aspect": "portrait9x16",
            "path": "/m/a.mp4"
        }))
        .unwrap();
        let s = req.settings();
        assert_eq!((s.width, s.height), (2160, 3840));
        assert_eq!(s.fps, EXPORT_FPS);
        assert_eq!(s.video_codec, VideoCodec::Hevc);
        assert_eq!(
            s.video_bitrate,
            ExportSettings::default_bitrate(2160, 3840, VideoCodec::Hevc)
        );
        s.validate().unwrap();
        let json = serde_json::to_value(ExportRequest {
            codec: VideoCodec::H264,
            resolution: ExportResolution::Hd1080,
            ..req
        })
        .unwrap();
        assert_eq!(json["codec"], "H264");
        assert_eq!(json["resolution"], "1080p");
    }

    #[test]
    fn eta_extrapolates_the_average_frame_time() {
        let s = Duration::from_secs;
        assert_eq!(eta(s(3), 0, 100), None);
        assert_eq!(eta(s(3), 10, 100), Some(s(27)));
        assert_eq!(eta(s(3), 100, 100), Some(s(0)));
    }

    #[test]
    fn events_serialise_tagged_by_state() {
        let p = ExportEvent::Progress {
            frame: 3,
            total_frames: 30,
            fraction: 0.1,
            eta_ms: Some(900),
        };
        let json = serde_json::to_value(&p).unwrap();
        assert_eq!(json["state"], "progress");
        assert_eq!(json["eta_ms"], 900);
        assert_eq!(
            serde_json::to_value(ExportEvent::Cancelled).unwrap(),
            serde_json::json!({ "state": "cancelled" })
        );
        assert!(!p.is_final() && ExportEvent::Cancelled.is_final());
    }

    #[test]
    fn partial_path_is_hidden_next_to_the_output() {
        assert_eq!(
            partial_path(Path::new("/m/Trip.mp4")),
            Path::new("/m/.Trip.mp4.partial")
        );
    }

    struct Codecs(bool);
    impl Encoder for Codecs {
        fn create(
            &self,
            _: &Path,
            _: &ExportSettings,
        ) -> kv_media::Result<Box<dyn kv_media::VideoWriter>> {
            unimplemented!()
        }
        fn supports(&self, c: VideoCodec) -> bool {
            c == VideoCodec::H264 || self.0
        }
    }

    fn with_edit(dir: &Path, present: bool) -> Session {
        let mut s = Session::new(dir.to_path_buf());
        if present {
            std::fs::write(dir.join("a.mp4"), b"x").unwrap();
        }
        s.apply(Command::AddMedia {
            item: MediaItem {
                id: MediaId(1),
                path: "a.mp4".into(),
                duration: Micros(5_000_000),
                kind: MediaKind::Video,
                hilights: vec![],
                clip_audio: false,
                chapters: vec![],
            },
        })
        .unwrap();
        let mut edl = kv_core::Edl {
            segments: vec![Segment {
                media_id: MediaId(1),
                src_in: Micros(0),
                src_out: Micros(1_000_000),
                timeline_start: Micros(0),
                transition: Transition::Cut,
                speed: Speed::NORMAL,
            }],
        };
        edl.repack();
        s.apply(Command::ReplaceEdl {
            section: Project::DEFAULT_SECTION,
            edl,
        })
        .unwrap();
        s
    }

    #[test]
    fn options_list_supported_codecs_and_block_empty_or_missing_edits() {
        let dir = tempfile::tempdir().unwrap();
        let sec = Project::DEFAULT_SECTION;
        let empty = Session::new(dir.path().to_path_buf());
        let o = empty.export_options(sec, &Codecs(false));
        assert_eq!(o.codecs, vec![VideoCodec::H264]);
        assert_eq!(o.file_name, "Untitled.mp4");
        assert_eq!(o.aspect, AspectRatio::Landscape16x9);
        assert_eq!(
            o.blocked.as_deref(),
            Some("Nothing to export yet: add clips and music to make an edit.")
        );
        assert!(empty
            .export_input(
                sec,
                &ExportRequest {
                    codec: VideoCodec::H264,
                    resolution: ExportResolution::Hd1080,
                    aspect: AspectRatio::Square1x1,
                    path: dir.path().join("x.mp4"),
                }
            )
            .is_err());

        let ready = with_edit(dir.path(), true);
        let o = ready.export_options(sec, &Codecs(true));
        assert_eq!(o.codecs, vec![VideoCodec::H264, VideoCodec::Hevc]);
        assert_eq!(o.blocked, None);
        assert!(ready.export_blocker(SectionId(9)).is_some());

        let other = tempfile::tempdir().unwrap();
        let missing = with_edit(other.path(), false);
        assert_eq!(
            missing.export_blocker(sec).as_deref(),
            Some("A clip in the edit is missing. Relink or remove it to export.")
        );
    }
}
