//! The export pipeline: one project section to an MP4 file.
//!
//! [`export`] ties the pieces together, platform-free (the backend and the
//! encoder are trait objects; the CLI passes `FfmpegBackend` for both):
//!
//! 1. Check everything that can fail cheaply *before* the output file is
//!    created: the section exists, its edit is not empty, every media item
//!    is in the project and probes, the settings are valid, and the music
//!    bed renders.
//! 2. For each frame of [`plan`]`(edl, settings.fps)`, fetch every layer's
//!    source frame with [`VideoReader::frame_at`], [`composite`] them with
//!    [`Fit::Fill`] and write the result. Readers are opened lazily, one per
//!    media item, decoded upright at [`source_size_for`] the output, and kept
//!    in a small LRU ([`MAX_OPEN_READERS`]).
//! 3. After each video frame, write the music samples up to that frame's end
//!    so audio and video stay interleaved for the muxer.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use kv_audio::{render_music_bed, MusicBedOptions, DEFAULT_FADE_OUT};
use kv_core::{MediaId, Micros, Project, SectionId};
use kv_media::{
    DecodeOptions, Encoder, ExportSettings, MediaBackend, MediaError, VideoFrame, VideoReader,
    VideoWriter,
};

use crate::{composite, frame_pts, plan, segment_end, source_size_for, CompositeError, Fit};
use crate::{OutputFrame, PlanError};

/// Most video readers kept open at once; the least recently used one is
/// closed when another is needed (guess: 4 covers a crossfade plus the next
/// clips without holding many decoders).
pub const MAX_OPEN_READERS: usize = 4;

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
    /// Invalid settings, or the encoder failed.
    #[error(transparent)]
    Media(#[from] MediaError),
    /// The frame plan could not be built.
    #[error(transparent)]
    Plan(#[from] PlanError),
    /// Compositing a frame failed.
    #[error(transparent)]
    Composite(#[from] CompositeError),
}

/// Write section `section` of `project` to an MP4 at `out`.
///
/// `MediaItem::path` and `MusicRef::path` are resolved relative to
/// `media_root` (normally the project file's directory). Video comes from
/// `media`, the file is written by `encoder`. Audio is the section's music
/// rendered with [`render_music_bed`] for the length of the edit at
/// `settings.audio_sample_rate` / `audio_channels`, with the project's
/// music volume and a [`MUSIC_FADE_OUT`] fade; without music the track is
/// silent (guess: players handle that better than no audio track).
///
/// `progress` is called after every frame. Every check that does not need
/// the output (missing media, empty edit, bad settings, unreadable music)
/// fails before `out` is created. If writing fails part-way, the partial
/// file is removed (best effort).
#[allow(clippy::too_many_arguments)]
pub fn export(
    project: &Project,
    section: SectionId,
    media_root: &Path,
    media: &dyn MediaBackend,
    encoder: &dyn Encoder,
    out: &Path,
    settings: &ExportSettings,
    progress: &mut dyn FnMut(Progress),
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
    let frame_plan = plan(edl, settings.fps)?;

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
            path,
            decode: DecodeOptions {
                size: (size != (0, 0)).then_some(size),
                apply_rotation: true,
            },
        });
    }

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
            render_music_bed(&path, &opts)
                .map_err(|source| RenderError::Music { path, source })?
                .samples
        }
        None => {
            let frames = audio_frames_at(duration, settings.audio_sample_rate);
            vec![0.0; frames * usize::from(settings.audio_channels)]
        }
    };

    let writer = encoder.create(out, settings)?;
    let mut job = Job {
        sources: &sources,
        media,
        readers: Vec::new(),
        settings,
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
    path: PathBuf,
    decode: DecodeOptions,
}

struct Job<'a> {
    sources: &'a [Source],
    media: &'a dyn MediaBackend,
    /// Open readers, most recently used last.
    readers: Vec<(MediaId, Box<dyn VideoReader>)>,
    settings: &'a ExportSettings,
}

impl Job<'_> {
    /// Write every frame and the audio, finish the file, return the
    /// encoder name.
    fn write_all(
        &mut self,
        mut writer: Box<dyn VideoWriter>,
        frame_plan: &crate::FramePlan<'_>,
        audio: &[f32],
        progress: &mut dyn FnMut(Progress),
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
            progress(Progress {
                frame: frame.index + 1,
                total_frames: total,
            });
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
        let reader = self.reader(src).map_err(source_err)?;
        if let Some(f) = reader.frame_at(t).map_err(source_err)? {
            return Ok(f);
        }
        // `t` is before the first frame (a stream starting late): show the
        // first frame.
        reader.seek(Micros(0)).map_err(source_err)?;
        reader
            .next_frame()
            .map_err(source_err)?
            .ok_or(RenderError::NoFrame { media_id, time: t })
    }

    /// The open reader for `src`, opening it (and closing the least recently
    /// used one beyond [`MAX_OPEN_READERS`]) if needed.
    fn reader(&mut self, src: &Source) -> Result<&mut Box<dyn VideoReader>, MediaError> {
        if let Some(i) = self.readers.iter().position(|(id, _)| *id == src.media_id) {
            let entry = self.readers.remove(i);
            self.readers.push(entry);
        } else {
            if self.readers.len() >= MAX_OPEN_READERS {
                self.readers.remove(0);
            }
            let r = self.media.open_video(&src.path, &src.decode)?;
            self.readers.push((src.media_id, r));
        }
        let last = self.readers.last_mut().expect("just pushed");
        Ok(&mut last.1)
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
    }

    struct FakeReader {
        info: VideoInfo,
        level: u8,
        size: (u32, u32),
        live: Arc<AtomicUsize>,
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
    }

    struct FakeWriter(Arc<Mutex<Vec<Call>>>);

    impl Encoder for FakeEncoder {
        fn create(
            &self,
            _: &Path,
            settings: &ExportSettings,
        ) -> kv_media::Result<Box<dyn VideoWriter>> {
            settings.validate()?;
            self.created.fetch_add(1, Ordering::SeqCst);
            Ok(Box::new(FakeWriter(self.calls.clone())))
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
            &mut |pr| progress.push(pr),
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
                &mut |_| {},
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
                &mut |_| {},
            ),
            Err(RenderError::Media(MediaError::InvalidExportSettings(_)))
        ));
        assert_eq!(enc.created.load(Ordering::SeqCst), 0, "no output created");
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
}
