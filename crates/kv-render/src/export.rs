//! The export pipeline: one project section to an MP4 file.
//!
//! [`export`] ties the pieces together, platform-free (the backend and the
//! encoder are trait objects; the CLI passes `FfmpegBackend` for both):
//!
//! 1. Check everything that can fail cheaply *before* the output file is
//!    created: the section exists, its edit is not empty, every media item
//!    is in the project and probes, the settings are valid, and the music
//!    bed renders.
//! 2. For each frame of [`plan`]`(edl, settings.fps, source rates)`, fetch
//!    every layer's source frame with [`VideoReader::frame_at`],
//!    [`composite`] them with [`Fit::Fill`] and write the result (a slow-mo
//!    in-between frame is a blend of two source frames). Readers are opened
//!    lazily, one per media item, decoded upright at [`source_size_for`] the
//!    output, and kept in a small LRU ([`MAX_OPEN_READERS`]). Each reader
//!    remembers its last [`FRAMES_KEPT`] frames by source time, so the frame
//!    shared by consecutive slow-mo blends is decoded once. A section title
//!    is rasterised once at the output size ([`title::rasterize`]) and
//!    alpha-blended over the frames the plan gives it an overlay on
//!    ([`overlay()`]).
//! 3. After each video frame, write the audio samples up to that frame's end
//!    so audio and video stay interleaved for the muxer. The audio is the
//!    music bed with the clip audio of every media item that has it on
//!    mixed under it ([`kv_audio::render_clip_audio`],
//!    [`kv_audio::mix_under_music`]).
//! 4. After each frame, report [`Progress`]; a callback returning
//!    [`ControlFlow::Break`] cancels the export ([`RenderError::Cancelled`])
//!    and the partial file is removed.

use std::ops::ControlFlow;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use kv_audio::{
    mix_under_music, render_clip_audio, render_music_bed, ClipAudioOptions, ClipAudioSource,
    MusicBedOptions, PcmAudio, DEFAULT_FADE_OUT,
};
use kv_core::{MediaId, Micros, Project, SectionId};
use kv_media::{
    open_item_video, Chapter, DecodeOptions, Encoder, ExportSettings, MediaBackend, MediaError,
    Rational, VideoFrame, VideoReader, VideoWriter,
};

use crate::{composite, frame_pts, overlay, plan, segment_end, source_size_for};
use crate::{title, CompositeError, Fit, OutputFrame, OverlayId, PlanError};

/// Most video readers kept open at once; the least recently used one is
/// closed when another is needed (guess: 4 covers a crossfade plus the next
/// clips without holding many decoders).
pub const MAX_OPEN_READERS: usize = 4;

/// Decoded frames kept per open reader, by requested source time: the two
/// frames of a slow-mo blend, so the next output frame reuses them.
const FRAMES_KEPT: usize = 2;

/// Fade-out at the end of the music bed: [`kv_audio::DEFAULT_FADE_OUT`]
/// (1 s, guess).
pub const MUSIC_FADE_OUT: Micros = DEFAULT_FADE_OUT;

/// Progress of a running export, reported after every written frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Progress {
    /// Frames written so far.
    pub frame: u64,
    /// Frames the export will write in total.
    pub total_frames: u64,
}

/// Summary of a finished export.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportReport {
    /// Video frames written.
    pub frames: u64,
    /// Length of the edit (and of the audio track).
    pub duration: Micros,
    /// Video encoder used, e.g. `"h264_videotoolbox"`.
    pub encoder_name: String,
    /// Wall-clock time the export took.
    pub elapsed: Duration,
}

/// Why an export failed.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum RenderError {
    /// The project has no section with this id.
    #[error("project has no section {0:?}")]
    SectionNotFound(SectionId),
    /// The section's edit has no segments.
    #[error("section {0:?} has an empty edit")]
    EmptyEdl(SectionId),
    /// A segment refers to a media item the project does not have.
    #[error("edit refers to media {0:?}, which is not in the project")]
    MissingMedia(MediaId),
    /// A media item's file is missing or cannot be probed or decoded.
    #[error("media {media_id:?} ({path:?}): {source}")]
    Source {
        media_id: MediaId,
        path: PathBuf,
        #[source]
        source: MediaError,
    },
    /// A media item has no video stream.
    #[error("media {media_id:?} ({path:?}) has no video stream")]
    NoVideo { media_id: MediaId, path: PathBuf },
    /// A reader returned no frame for a source time, even from the start.
    #[error("media {media_id:?} has no frame at {time:?}")]
    NoFrame { media_id: MediaId, time: Micros },
    /// The music file is missing or cannot be decoded.
    #[error("music {path:?}: {source}")]
    Music {
        path: PathBuf,
        #[source]
        source: kv_audio::Error,
    },
    /// A clip's audio could not be decoded.
    #[error(transparent)]
    ClipAudio(kv_audio::Error),
    /// Invalid settings, or the encoder failed.
    #[error(transparent)]
    Media(#[from] MediaError),
    /// The frame plan could not be built.
    #[error(transparent)]
    Plan(#[from] PlanError),
    /// Compositing a frame failed.
    #[error(transparent)]
    Composite(#[from] CompositeError),
    /// The progress callback asked to stop.
    #[error("export cancelled")]
    Cancelled,
}

/// Write section `section` of `project` to an MP4 at `out`.
///
/// `MediaItem::path` and `MusicRef::path` are resolved relative to
/// `media_root` (normally the project file's directory). Video comes from
/// `media`, the file is written by `encoder`. Audio is the section's music
/// rendered with [`render_music_bed`] for the length of the edit at
/// `settings.audio_sample_rate` / `audio_channels`, with the project's
/// music volume and a [`MUSIC_FADE_OUT`] fade; without music the track is
/// silent (guess: players handle that better than no audio track). The
/// clip audio of media items with `clip_audio` on is mixed under the music
/// at full level, through a limiter ([`kv_audio::mix_under_music`]).
///
/// `progress` is called after every frame; returning
/// [`ControlFlow::Break`] stops the export with [`RenderError::Cancelled`].
/// Every check that does not need
/// the output (missing media, empty edit, bad settings, unreadable music
/// or clip audio)
/// fails before `out` is created. If writing fails part-way, the partial
/// file is removed (best effort); so is a cancelled export's.
#[allow(clippy::too_many_arguments)]
pub fn export(
    project: &Project,
    section: SectionId,
    media_root: &Path,
    media: &dyn MediaBackend,
    encoder: &dyn Encoder,
    out: &Path,
    settings: &ExportSettings,
    progress: &mut dyn FnMut(Progress) -> ControlFlow<()>,
) -> Result<ExportReport, RenderError> {
    let started = Instant::now();
    let sec = project
        .section(section)
        .ok_or(RenderError::SectionNotFound(section))?;
    let edl = &sec.edit;
    if edl.segments.is_empty() {
        return Err(RenderError::EmptyEdl(section));
    }
    settings.validate()?;

    // Resolve and probe every media item the edit uses, in first-use order.
    let mut sources: Vec<Source> = Vec::new();
    for seg in &edl.segments {
        if sources.iter().any(|s| s.media_id == seg.media_id) {
            continue;
        }
        let item = project
            .media_item(seg.media_id)
            .ok_or(RenderError::MissingMedia(seg.media_id))?;
        let path = resolve(media_root, &item.path);
        let info = media.probe(&path).map_err(|source| RenderError::Source {
            media_id: item.id,
            path: path.clone(),
            source,
        })?;
        let Some(video) = info.video else {
            return Err(RenderError::NoVideo {
                media_id: item.id,
                path,
            });
        };
        let (w, h) = if video.rotation % 180 == 90 {
            (video.height, video.width)
        } else {
            (video.width, video.height)
        };
        let size = source_size_for(settings.width, settings.height, w, h, Fit::Fill);
        sources.push(Source {
            media_id: item.id,
            fps: video.fps,
            path,
            chapters: Chapter::from_refs(&item.chapters, |p| resolve(media_root, p)),
            decode: DecodeOptions {
                size: (size != (0, 0)).then_some(size),
                apply_rotation: true,
            },
        });
    }

    let source_fps = |id: MediaId| sources.iter().find(|s| s.media_id == id).map(|s| s.fps);
    let frame_plan = plan(edl, settings.fps, &source_fps)?.with_title(sec.title.as_ref());
    let title = sec
        .title
        .as_ref()
        .map(|t| title::rasterize(t, settings.width, settings.height));

    // Audio, rendered up front so a bad music file fails before the output
    // exists.
    let duration = Micros(edl.segments.iter().map(segment_end).max().unwrap_or(0));
    let audio = match &sec.music {
        Some(music) => {
            let path = resolve(media_root, &music.path);
            let mut opts = MusicBedOptions::new(
                settings.audio_sample_rate,
                settings.audio_channels,
                duration,
            );
            opts.volume_percent = project.settings.music_volume_percent;
            opts.fade_out = MUSIC_FADE_OUT;
            render_music_bed(&path, &opts).map_err(|source| RenderError::Music { path, source })?
        }
        None => {
            let frames = audio_frames_at(duration, settings.audio_sample_rate);
            PcmAudio {
                samples: vec![0.0; frames * usize::from(settings.audio_channels)],
                sample_rate: settings.audio_sample_rate,
                channels: settings.audio_channels,
            }
        }
    };
    let audio = mix_clip_audio(project, edl, &sources, audio, duration)?.samples;

    let writer = encoder.create(out, settings)?;
    let mut job = Job {
        sources: &sources,
        media,
        readers: Vec::new(),
        settings,
        title,
    };
    match job.write_all(writer, &frame_plan, &audio, progress) {
        Ok(encoder_name) => Ok(ExportReport {
            frames: frame_plan.len(),
            duration,
            encoder_name,
            elapsed: started.elapsed(),
        }),
        Err(e) => {
            // Close the readers and the writer first, then drop the partial
            // file; failing to remove it is not worth masking `e`.
            drop(job);
            let _ = std::fs::remove_file(out);
            Err(e)
        }
    }
}

/// `bed` with the clip audio of the edit's media that have `clip_audio` on
/// mixed under it; `bed` unchanged when none has.
fn mix_clip_audio(
    project: &Project,
    edl: &kv_core::Edl,
    sources: &[Source],
    bed: PcmAudio,
    duration: Micros,
) -> Result<PcmAudio, RenderError> {
    let clip_sources: Vec<ClipAudioSource> = sources
        .iter()
        .filter(|s| project.media_item(s.media_id).is_some_and(|m| m.clip_audio))
        .map(|s| ClipAudioSource {
            media_id: s.media_id,
            path: s.path.clone(),
        })
        .collect();
    if clip_sources.is_empty() {
        return Ok(bed);
    }
    let opts = ClipAudioOptions {
        sample_rate: bed.sample_rate,
        channels: bed.channels,
        length: duration,
    };
    let clip = render_clip_audio(edl, &clip_sources, &opts).map_err(RenderError::ClipAudio)?;
    mix_under_music(&bed, &clip).map_err(RenderError::ClipAudio)
}

/// `root / rel`, where `rel` is `/`-separated (an absolute `rel` wins).
fn resolve(root: &Path, rel: &str) -> PathBuf {
    rel.split('/').filter(|p| !p.is_empty()).fold(
        if rel.starts_with('/') {
            PathBuf::from("/")
        } else {
            root.to_path_buf()
        },
        |acc, part| acc.join(part),
    )
}

/// Audio frames (samples per channel) before time `t`: `round(t * rate)`,
/// matching the length [`render_music_bed`] produces.
fn audio_frames_at(t: Micros, rate: u32) -> usize {
    let n = (i128::from(t.0.max(0)) * i128::from(rate) + 500_000) / 1_000_000;
    usize::try_from(n).unwrap_or(usize::MAX)
}

struct Source {
    media_id: MediaId,
    /// Frame rate from the probe, for snapping and slow-mo blending.
    fps: Rational,
    /// First file of the item (probed for `fps` and the decode size).
    path: PathBuf,
    /// The item's chapter files; empty for a single-file item.
    chapters: Vec<Chapter>,
    decode: DecodeOptions,
}

struct Job<'a> {
    sources: &'a [Source],
    media: &'a dyn MediaBackend,
    /// Open readers, most recently used last.
    readers: Vec<OpenReader<'a>>,
    settings: &'a ExportSettings,
    /// The section's title card at the output size, if it has one.
    title: Option<VideoFrame>,
}

struct OpenReader<'a> {
    media_id: MediaId,
    reader: Box<dyn VideoReader + 'a>,
    /// The last [`FRAMES_KEPT`] frames returned, by requested time, most
    /// recent last.
    recent: Vec<(Micros, VideoFrame)>,
}

impl<'a> Job<'a> {
    /// Write every frame and the audio, finish the file, return the
    /// encoder name.
    fn write_all(
        &mut self,
        mut writer: Box<dyn VideoWriter>,
        frame_plan: &crate::FramePlan<'_>,
        audio: &[f32],
        progress: &mut dyn FnMut(Progress) -> ControlFlow<()>,
    ) -> Result<String, RenderError> {
        let s = self.settings;
        let channels = usize::from(s.audio_channels);
        let total = frame_plan.len();
        let mut out = VideoFrame::solid(Micros(0), s.width, s.height, [0, 0, 0, 255]);
        let mut audio_sent = 0usize;
        for frame in frame_plan.iter() {
            self.render(&frame, &mut out)?;
            writer.write_video(&out)?;
            // Audio up to the end of this frame: about one frame's worth.
            let end = Micros(frame_pts(frame.index + 1, s.fps));
            let upto = (audio_frames_at(end, s.audio_sample_rate) * channels).min(audio.len());
            if upto > audio_sent {
                writer.write_audio(&audio[audio_sent..upto])?;
                audio_sent = upto;
            }
            let flow = progress(Progress {
                frame: frame.index + 1,
                total_frames: total,
            });
            if flow.is_break() {
                tracing::info!(frame = frame.index + 1, total, "export cancelled");
                return Err(RenderError::Cancelled);
            }
        }
        if audio_sent < audio.len() {
            writer.write_audio(&audio[audio_sent..])?;
        }
        let name = writer.encoder_name().to_owned();
        writer.finish()?;
        Ok(name)
    }

    fn render(&mut self, frame: &OutputFrame, out: &mut VideoFrame) -> Result<(), RenderError> {
        let mut decoded = Vec::with_capacity(frame.layers.len());
        for layer in &frame.layers {
            decoded.push((self.fetch(layer.media_id, layer.src_time)?, layer.weight));
        }
        let layers: Vec<(&VideoFrame, f32)> = decoded.iter().map(|(f, w)| (f, *w)).collect();
        out.pts = frame.pts;
        composite(&layers, out, Fit::Fill)?;
        for o in &frame.overlays {
            let image = match o.id {
                OverlayId::Title => self.title.as_ref(),
            };
            if let Some(image) = image {
                overlay(out, image, o.opacity)?;
            }
        }
        Ok(())
    }

    /// The frame of `media_id` displayed at `t`.
    fn fetch(&mut self, media_id: MediaId, t: Micros) -> Result<VideoFrame, RenderError> {
        let src = self
            .sources
            .iter()
            .find(|s| s.media_id == media_id)
            .ok_or(RenderError::MissingMedia(media_id))?;
        let source_err = |source| RenderError::Source {
            media_id,
            path: src.path.clone(),
            source,
        };
        let open = self.reader(src).map_err(source_err)?;
        if let Some((_, f)) = open.recent.iter().find(|(at, _)| *at == t) {
            return Ok(f.clone());
        }
        let reader = &mut open.reader;
        let frame = match reader.frame_at(t).map_err(source_err)? {
            Some(f) => f,
            None => {
                // `t` is before the first frame (a stream starting late):
                // show the first frame.
                reader.seek(Micros(0)).map_err(source_err)?;
                reader
                    .next_frame()
                    .map_err(source_err)?
                    .ok_or(RenderError::NoFrame { media_id, time: t })?
            }
        };
        if open.recent.len() >= FRAMES_KEPT {
            open.recent.remove(0);
        }
        open.recent.push((t, frame.clone()));
        Ok(frame)
    }

    /// The open reader for `src`, opening it (and closing the least recently
    /// used one beyond [`MAX_OPEN_READERS`]) if needed.
    fn reader(&mut self, src: &Source) -> Result<&mut OpenReader<'a>, MediaError> {
        if let Some(i) = self.readers.iter().position(|r| r.media_id == src.media_id) {
            let entry = self.readers.remove(i);
            self.readers.push(entry);
        } else {
            if self.readers.len() >= MAX_OPEN_READERS {
                self.readers.remove(0);
            }
            let reader = open_item_video(self.media, &src.path, &src.chapters, &src.decode)?;
            self.readers.push(OpenReader {
                media_id: src.media_id,
                reader,
                recent: Vec::with_capacity(FRAMES_KEPT),
            });
        }
        Ok(self.readers.last_mut().expect("just pushed"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use kv_core::{Edl, MediaItem, MediaKind, Segment, Speed, Transition};
    use kv_media::{MediaInfo, Rational, VideoInfo};

    /// Media id `n` is a solid frame of grey level `10 * n`.
    #[derive(Default)]
    struct FakeMedia {
        live: Arc<AtomicUsize>,
        max_live: Arc<AtomicUsize>,
        opens: Arc<Mutex<Vec<(PathBuf, DecodeOptions)>>>,
        probes: Arc<AtomicUsize>,
        /// Every `frame_at` time asked of any reader, in order.
        frame_ats: Arc<Mutex<Vec<Micros>>>,
    }

    struct FakeReader {
        info: VideoInfo,
        level: u8,
        size: (u32, u32),
        live: Arc<AtomicUsize>,
        frame_ats: Arc<Mutex<Vec<Micros>>>,
    }

    impl Drop for FakeReader {
        fn drop(&mut self) {
            self.live.fetch_sub(1, Ordering::SeqCst);
        }
    }

    fn info() -> VideoInfo {
        VideoInfo {
            codec: "fake".into(),
            width: 64,
            height: 48,
            fps: Rational::new(30, 1),
            bit_depth: 8,
            rotation: 0,
        }
    }

    impl MediaBackend for FakeMedia {
        fn probe(&self, path: &Path) -> kv_media::Result<MediaInfo> {
            self.probes.fetch_add(1, Ordering::SeqCst);
            if path.to_string_lossy().contains("missing") {
                return Err(MediaError::Open {
                    path: path.to_path_buf(),
                    message: "no such file".into(),
                });
            }
            Ok(MediaInfo {
                duration: Micros(10_000_000),
                container: "fake".into(),
                video: Some(info()),
                audio: None,
                creation_time: None,
            })
        }

        fn open_video(
            &self,
            path: &Path,
            opts: &DecodeOptions,
        ) -> kv_media::Result<Box<dyn VideoReader>> {
            self.opens.lock().unwrap().push((path.to_path_buf(), *opts));
            let n = self.live.fetch_add(1, Ordering::SeqCst) + 1;
            self.max_live.fetch_max(n, Ordering::SeqCst);
            let id: u8 = path
                .file_stem()
                .unwrap()
                .to_string_lossy()
                .trim_start_matches("clip")
                .parse()
                .unwrap();
            Ok(Box::new(FakeReader {
                info: info(),
                level: id * 10,
                size: opts.size.unwrap_or((64, 48)),
                live: self.live.clone(),
                frame_ats: self.frame_ats.clone(),
            }))
        }
    }

    impl VideoReader for FakeReader {
        fn info(&self) -> &VideoInfo {
            &self.info
        }
        fn seek(&mut self, _: Micros) -> kv_media::Result<()> {
            Ok(())
        }
        fn next_frame(&mut self) -> kv_media::Result<Option<VideoFrame>> {
            self.frame_at(Micros(0))
        }
        fn frame_at(&mut self, t: Micros) -> kv_media::Result<Option<VideoFrame>> {
            self.frame_ats.lock().unwrap().push(t);
            let l = self.level;
            Ok(Some(VideoFrame::solid(
                t,
                self.size.0,
                self.size.1,
                [l, l, l, 255],
            )))
        }
    }

    #[derive(Debug, Clone, PartialEq)]
    enum Call {
        Video { pts: Micros, level: u8 },
        Audio(usize),
        Finish,
    }

    #[derive(Default, Clone)]
    struct FakeEncoder {
        calls: Arc<Mutex<Vec<Call>>>,
        created: Arc<AtomicUsize>,
        /// Every video frame written, in order.
        frames: Arc<Mutex<Vec<VideoFrame>>>,
    }

    struct FakeWriter(Arc<Mutex<Vec<Call>>>, Arc<Mutex<Vec<VideoFrame>>>);

    impl Encoder for FakeEncoder {
        fn create(
            &self,
            _: &Path,
            settings: &ExportSettings,
        ) -> kv_media::Result<Box<dyn VideoWriter>> {
            settings.validate()?;
            self.created.fetch_add(1, Ordering::SeqCst);
            Ok(Box::new(FakeWriter(
                self.calls.clone(),
                self.frames.clone(),
            )))
        }

        fn supports(&self, _: kv_media::VideoCodec) -> bool {
            true
        }
    }

    impl VideoWriter for FakeWriter {
        fn encoder_name(&self) -> &str {
            "fake264"
        }
        fn write_video(&mut self, f: &VideoFrame) -> kv_media::Result<()> {
            self.0.lock().unwrap().push(Call::Video {
                pts: f.pts,
                level: f.data[0],
            });
            self.1.lock().unwrap().push(f.clone());
            Ok(())
        }
        fn write_audio(&mut self, s: &[f32]) -> kv_media::Result<()> {
            assert!(s.iter().all(|&v| v == 0.0), "no music: silence");
            self.0.lock().unwrap().push(Call::Audio(s.len()));
            Ok(())
        }
        fn finish(self: Box<Self>) -> kv_media::Result<()> {
            self.0.lock().unwrap().push(Call::Finish);
            Ok(())
        }
    }

    fn settings() -> ExportSettings {
        ExportSettings {
            width: 32,
            height: 18,
            fps: Rational::new(10, 1),
            audio_sample_rate: 1_000,
            audio_channels: 2,
            ..ExportSettings::default()
        }
    }

    /// Media ids `1..=n` at `clipN.mp4`; one 1 s cut per entry of `order`.
    fn project(n: u64, order: &[u64]) -> Project {
        let mut p = Project::new(kv_core::ProjectId(1));
        for id in 1..=n {
            p.media.push(MediaItem {
                id: MediaId(id),
                path: format!("clips/clip{id}.mp4"),
                duration: Micros(10_000_000),
                kind: MediaKind::Video,
                hilights: vec![],
                clip_audio: false,
                chapters: Vec::new(),
            });
        }
        let mut edl = Edl {
            segments: order
                .iter()
                .map(|&id| Segment {
                    media_id: MediaId(id),
                    src_in: Micros(0),
                    src_out: Micros(1_000_000),
                    timeline_start: Micros(0),
                    transition: Transition::Cut,
                    speed: Speed::NORMAL,
                })
                .collect(),
        };
        edl.repack();
        p.sections[0].edit = edl;
        p
    }

    fn run(
        p: &Project,
        media: &FakeMedia,
        enc: &FakeEncoder,
        progress: &mut Vec<Progress>,
    ) -> Result<ExportReport, RenderError> {
        export(
            p,
            Project::DEFAULT_SECTION,
            Path::new("/root"),
            media,
            enc,
            Path::new("/nonexistent/out.mp4"),
            &settings(),
            &mut |pr| {
                progress.push(pr);
                ControlFlow::Continue(())
            },
        )
    }

    #[test]
    fn writes_every_frame_with_interleaved_silent_audio() {
        let p = project(2, &[1, 2]);
        let (media, enc) = (FakeMedia::default(), FakeEncoder::default());
        let mut progress = Vec::new();
        let report = run(&p, &media, &enc, &mut progress).unwrap();
        assert_eq!(report.frames, 20);
        assert_eq!(report.duration, Micros(2_000_000));
        assert_eq!(report.encoder_name, "fake264");

        let calls = enc.calls.lock().unwrap().clone();
        assert_eq!(calls.last(), Some(&Call::Finish));
        // Video and audio alternate: 100 audio frames (200 samples) per video frame.
        for (k, pair) in calls[..40].chunks(2).enumerate() {
            let level = if k < 10 { 10 } else { 20 };
            assert_eq!(
                pair,
                [
                    Call::Video {
                        pts: Micros(k as i64 * 100_000),
                        level
                    },
                    Call::Audio(200)
                ],
                "frame {k}"
            );
        }
        assert_eq!(calls.len(), 41);
        let progress_frames: Vec<u64> = progress.iter().map(|p| p.frame).collect();
        assert_eq!(progress_frames, (1..=20).collect::<Vec<_>>());
        assert!(progress.iter().all(|p| p.total_frames == 20));

        // Paths resolved against the media root, decoded at the fill size.
        let opens = media.opens.lock().unwrap();
        assert_eq!(opens.len(), 2);
        assert_eq!(opens[0].0, Path::new("/root/clips/clip1.mp4"));
        // 64x48 (4:3) into 32x18 (16:9): width-bound, 32x24.
        assert_eq!(opens[0].1.size, Some((32, 24)));
        assert!(opens[0].1.apply_rotation);
    }

    #[test]
    fn readers_are_reused_and_capped_by_lru() {
        // 6 media; 1 is reused right after others, 1..4 fit in the cache.
        let order = [1, 2, 1, 3, 4, 1, 5, 6, 2];
        let p = project(6, &order);
        let (media, enc) = (FakeMedia::default(), FakeEncoder::default());
        run(&p, &media, &enc, &mut Vec::new()).unwrap();
        assert!(media.max_live.load(Ordering::SeqCst) <= MAX_OPEN_READERS);
        assert_eq!(media.live.load(Ordering::SeqCst), 0, "all readers closed");
        let opened: Vec<String> = media
            .opens
            .lock()
            .unwrap()
            .iter()
            .map(|(p, _)| p.file_stem().unwrap().to_string_lossy().into_owned())
            .collect();
        // 1,2,3,4 open; 1 stays hot; 5 evicts 2, 6 evicts 3; 2 evicts 4.
        assert_eq!(
            opened,
            ["clip1", "clip2", "clip3", "clip4", "clip5", "clip6", "clip2"]
        );
        // Each media probed once.
        assert_eq!(media.probes.load(Ordering::SeqCst), 6);
    }

    #[test]
    fn slow_motion_decodes_each_source_frame_once() {
        // 1 s of a 30 fps source at 50 %, output at 10 fps: source times
        // 0, 50 ms, 100 ms, ...: on frames 0, 3, 6, ... and halfway between
        // frames 1 and 2, 4 and 5, ...
        let mut p = project(1, &[1]);
        p.sections[0].edit.segments[0].speed = Speed(50);
        p.sections[0].edit.repack();
        let (media, enc) = (FakeMedia::default(), FakeEncoder::default());
        let report = run(&p, &media, &enc, &mut Vec::new()).unwrap();
        assert_eq!(report.frames, 20);

        let fps = Rational::new(30, 1);
        let source_fps = |_: MediaId| Some(fps);
        let edl = &p.sections[0].edit;
        let frame_plan = plan(edl, Rational::new(10, 1), &source_fps).unwrap();
        let blends = frame_plan.iter().filter(|f| f.layers.len() == 2).count();
        assert_eq!(blends, 10, "every other frame is a blend");
        let mut wanted: Vec<Micros> = frame_plan
            .iter()
            .flat_map(|f| f.layers.into_iter().map(|l| l.src_time))
            .collect();
        wanted.dedup();
        // The frame shared by consecutive output frames comes from the
        // cache: the reader sees each source time once, in order.
        let asked = media.frame_ats.lock().unwrap().clone();
        assert_eq!(asked, wanted);
        assert!(asked.windows(2).all(|w| w[0] < w[1]));
        assert_eq!(asked[..4], [0, 33_333, 66_667, 100_000].map(Micros));
    }

    #[test]
    fn title_is_drawn_over_the_first_frames_only() {
        // Two 1 s cuts of dark grey footage at 10 fps, 320x180, a 1 s title.
        let mut p = project(2, &[1, 2]);
        p.sections[0].title = Some(kv_core::TitleCard {
            text: "Summer".into(),
            duration: Micros(1_000_000),
        });
        let s = ExportSettings {
            width: 320,
            height: 180,
            ..settings()
        };
        let (media, enc) = (FakeMedia::default(), FakeEncoder::default());
        let report = export(
            &p,
            Project::DEFAULT_SECTION,
            Path::new("/root"),
            &media,
            &enc,
            Path::new("/nonexistent/out.mp4"),
            &s,
            &mut |_| ControlFlow::Continue(()),
        )
        .unwrap();
        assert_eq!(report.frames, 20);
        let frames = enc.frames.lock().unwrap();
        // Bright pixels in the central band (text), per frame.
        let bright = |f: &VideoFrame| {
            let mut n = 0;
            for y in 60..120usize {
                for x in 32..288usize {
                    let i = (y * 320 + x) * 4;
                    if f.data[i..i + 3].iter().all(|&c| c > 200) {
                        n += 1;
                    }
                }
            }
            n
        };
        let counts: Vec<usize> = frames.iter().map(bright).collect();
        // Frame 0: opacity 0. Frames 3..=7: fully shown. From 1 s: none.
        assert_eq!(counts[0], 0, "{counts:?}");
        assert!(counts[3..=7].iter().all(|&n| n > 40), "{counts:?}");
        assert!(counts[10..].iter().all(|&n| n == 0), "{counts:?}");
        // Corners keep the footage (grey 10, then 20) throughout.
        for (k, f) in frames.iter().enumerate() {
            let level = if k < 10 { 10 } else { 20 };
            assert_eq!(f.data[..4], [level, level, level, 255], "frame {k}");
        }
        // After the title the frames are untouched footage.
        assert!(frames[10..]
            .iter()
            .all(|f| f.data.chunks_exact(4).all(|px| px == [20, 20, 20, 255])));
    }

    #[test]
    fn errors_before_the_output_is_created() {
        let enc = FakeEncoder::default();
        let media = FakeMedia::default();

        let empty = project(1, &[]);
        assert!(matches!(
            run(&empty, &media, &enc, &mut Vec::new()),
            Err(RenderError::EmptyEdl(_))
        ));

        let mut unknown = project(1, &[1]);
        unknown.sections[0].edit.segments[0].media_id = MediaId(9);
        assert!(matches!(
            run(&unknown, &media, &enc, &mut Vec::new()),
            Err(RenderError::MissingMedia(MediaId(9)))
        ));

        let mut missing = project(2, &[1, 2]);
        missing.media[1].path = "missing/clip2.mp4".into();
        match run(&missing, &media, &enc, &mut Vec::new()) {
            Err(RenderError::Source {
                media_id,
                path,
                source: MediaError::Open { .. },
            }) => {
                assert_eq!(media_id, MediaId(2));
                assert_eq!(path, Path::new("/root/missing/clip2.mp4"));
            }
            other => panic!("{other:?}"),
        }

        let mut music = project(1, &[1]);
        music.sections[0].music = Some(kv_core::MusicRef {
            path: "no/such/song.wav".into(),
            duration: Micros(1_000_000),
        });
        assert!(matches!(
            run(&music, &media, &enc, &mut Vec::new()),
            Err(RenderError::Music { .. })
        ));

        // Clip audio on: the fake paths do not exist, so decoding fails.
        let mut clip_audio = project(2, &[1, 2]);
        clip_audio.media[1].clip_audio = true;
        match run(&clip_audio, &media, &enc, &mut Vec::new()) {
            Err(RenderError::ClipAudio(kv_audio::Error::ClipSource { media_id, path, .. })) => {
                assert_eq!(media_id, MediaId(2));
                assert_eq!(path, Path::new("/root/clips/clip2.mp4"));
            }
            other => panic!("{other:?}"),
        }

        let p = project(1, &[1]);
        assert!(matches!(
            export(
                &p,
                SectionId(42),
                Path::new("/root"),
                &media,
                &enc,
                Path::new("/x.mp4"),
                &settings(),
                &mut |_| ControlFlow::Continue(()),
            ),
            Err(RenderError::SectionNotFound(SectionId(42)))
        ));

        let bad = ExportSettings {
            width: 31,
            ..settings()
        };
        assert!(matches!(
            export(
                &p,
                Project::DEFAULT_SECTION,
                Path::new("/root"),
                &media,
                &enc,
                Path::new("/x.mp4"),
                &bad,
                &mut |_| ControlFlow::Continue(()),
            ),
            Err(RenderError::Media(MediaError::InvalidExportSettings(_)))
        ));
        assert_eq!(enc.created.load(Ordering::SeqCst), 0, "no output created");
    }

    #[test]
    fn break_from_progress_cancels_and_removes_the_partial_file() {
        let p = project(2, &[1, 2]);
        let (media, enc) = (FakeMedia::default(), FakeEncoder::default());
        // The fake encoder writes nothing; stand in for its partial file.
        let out = std::env::temp_dir().join(format!("kv-render-cancel-{}.mp4", std::process::id()));
        std::fs::write(&out, b"partial").unwrap();
        let mut seen = Vec::new();
        let err = export(
            &p,
            Project::DEFAULT_SECTION,
            Path::new("/root"),
            &media,
            &enc,
            &out,
            &settings(),
            &mut |pr| {
                seen.push(pr.frame);
                if pr.frame == 5 {
                    ControlFlow::Break(())
                } else {
                    ControlFlow::Continue(())
                }
            },
        )
        .unwrap_err();
        assert!(matches!(err, RenderError::Cancelled), "{err:?}");
        assert_eq!(seen, vec![1, 2, 3, 4, 5], "stops right after the break");
        let calls = enc.calls.lock().unwrap().clone();
        assert!(!calls.contains(&Call::Finish), "not finalised");
        assert_eq!(
            calls
                .iter()
                .filter(|c| matches!(c, Call::Video { .. }))
                .count(),
            5
        );
        assert!(!out.exists(), "partial file removed");
    }

    #[test]
    fn resolve_joins_slash_separated_paths() {
        assert_eq!(
            resolve(Path::new("/p"), "a/b.mp4"),
            Path::new("/p").join("a").join("b.mp4")
        );
        assert_eq!(
            resolve(Path::new("/p"), "/abs/c.mp4"),
            Path::new("/abs/c.mp4")
        );
    }

    #[test]
    fn audio_frames_round_like_the_music_bed() {
        assert_eq!(audio_frames_at(Micros(1_000_000), 48_000), 48_000);
        assert_eq!(audio_frames_at(Micros(33_333), 48_000), 1_600);
        assert_eq!(audio_frames_at(Micros(-5), 48_000), 0);
    }

    #[test]
    fn a_chaptered_item_decodes_across_its_chapter_files() {
        // Item 1 is two 1.5 s chapters (`clip1.mp4`, `clip2.mp4`); the cut
        // takes 1 s..2 s of it, across the boundary.
        let mut p = project(1, &[1]);
        p.media[0].duration = Micros(3_000_000);
        p.media[0].chapters = ["clips/clip1.mp4", "clips/clip2.mp4"]
            .map(|path| kv_core::ChapterRef {
                path: path.into(),
                duration: Micros(1_500_000),
            })
            .to_vec();
        let seg = &mut p.sections[0].edit.segments[0];
        seg.src_in = Micros(1_000_000);
        seg.src_out = Micros(2_000_000);
        let (media, enc) = (FakeMedia::default(), FakeEncoder::default());
        run(&p, &media, &enc, &mut Vec::new()).unwrap();

        let levels: Vec<u8> = enc
            .calls
            .lock()
            .unwrap()
            .iter()
            .filter_map(|c| match c {
                Call::Video { level, .. } => Some(*level),
                _ => None,
            })
            .collect();
        assert_eq!(levels, [[10; 5], [20; 5]].concat());
        let opened: Vec<PathBuf> = media
            .opens
            .lock()
            .unwrap()
            .iter()
            .map(|o| o.0.clone())
            .collect();
        assert_eq!(
            opened,
            [
                PathBuf::from("/root/clips/clip1.mp4"),
                PathBuf::from("/root/clips/clip2.mp4")
            ]
        );
        // Chapter-local times: 1.0..1.4 s in the first file, 0..0.4 s in
        // the second.
        let asked = media.frame_ats.lock().unwrap().clone();
        let want: Vec<Micros> = (10..15).chain(0..5).map(|k| Micros(k * 100_000)).collect();
        assert_eq!(asked, want);
    }
}
