//! Filmstrip thumbnails for the media library: the generator behind
//! [`Job::Filmstrip`](crate::Job::Filmstrip).
//!
//! [`FilmstripGenerator`] is injected through
//! [`JobContext`](crate::JobContext) so tests can use a fake;
//! [`MediaFilmstrips`] is the real one, decoding through a
//! [`MediaBackend`].

use std::error::Error;
use std::path::PathBuf;
use std::sync::Arc;

use kv_core::Micros;
use kv_media::{
    encode_jpeg, filmstrip_size, DecodeOptions, MediaBackend, VideoReader, FILMSTRIP_KEYFRAME_SNAP,
};

/// Thumbnails per filmstrip (guess: enough to recognise a clip in the
/// library without decoding much).
pub const FILMSTRIP_COUNT: u32 = 12;
/// Thumbnail height in pixels (guess: a library row at 1x and 2x scale).
pub const FILMSTRIP_HEIGHT: u32 = 90;
/// JPEG quality of stored thumbnails.
pub const FILMSTRIP_JPEG_QUALITY: u8 = 80;

/// Makes the JPEG thumbnails of one media item.
pub trait FilmstripGenerator: Send + Sync {
    /// `count` JPEG thumbnails `height` pixels high, evenly spaced over the
    /// item made of `files` (one file, or a chaptered recording's chapters in
    /// order, each with its duration). Thumbnail `i` shows the moment
    /// `total * (i + 0.5) / count` of the whole item.
    fn generate(
        &self,
        files: &[(PathBuf, Micros)],
        count: u32,
        height: u32,
    ) -> Result<Vec<Vec<u8>>, Box<dyn Error + Send + Sync>>;
}

/// The real generator: decodes with a [`MediaBackend`], scaled by the
/// decoder to [`filmstrip_size`], and encodes each frame as JPEG.
#[derive(Clone)]
pub struct MediaFilmstrips(pub Arc<dyn MediaBackend>);

impl std::fmt::Debug for MediaFilmstrips {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MediaFilmstrips").finish_non_exhaustive()
    }
}

impl FilmstripGenerator for MediaFilmstrips {
    fn generate(
        &self,
        files: &[(PathBuf, Micros)],
        count: u32,
        height: u32,
    ) -> Result<Vec<Vec<u8>>, Box<dyn Error + Send + Sync>> {
        let mut thumbs = Vec::with_capacity(count as usize);
        for (chapter, times) in cell_times(files, count) {
            if times.is_empty() {
                continue;
            }
            let path = &files[chapter].0;
            let info = self.0.probe(path)?;
            let video = info
                .video
                .ok_or_else(|| format!("{} has no video stream", path.display()))?;
            let opts = DecodeOptions {
                size: Some(filmstrip_size(&video, height)?),
                apply_rotation: true,
            };
            let mut reader = self.0.open_video(path, &opts)?;
            for t in times {
                let frame = frame_near_or_first(reader.as_mut(), t)?;
                thumbs.push(encode_jpeg(&frame, FILMSTRIP_JPEG_QUALITY)?);
            }
        }
        if thumbs.is_empty() {
            return Err("no thumbnails: the item has no files".into());
        }
        Ok(thumbs)
    }
}

fn frame_near_or_first(
    reader: &mut dyn VideoReader,
    t: Micros,
) -> Result<kv_media::VideoFrame, Box<dyn Error + Send + Sync>> {
    match reader.frame_near(t, FILMSTRIP_KEYFRAME_SNAP)? {
        Some(f) => Ok(f),
        // `t` is before the first frame (a stream starting late).
        None => Ok(reader.next_frame()?.ok_or("the stream has no frames")?),
    }
}

/// For each file (by index), the local times of the thumbnails that fall in
/// it, ascending: cell centre `i` of the whole item is
/// `total * (2i + 1) / (2 count)`, mapped into the chapter that contains it
/// (the last chapter for anything at or past the end).
pub(crate) fn cell_times(files: &[(PathBuf, Micros)], count: u32) -> Vec<(usize, Vec<Micros>)> {
    let mut out: Vec<(usize, Vec<Micros>)> = (0..files.len()).map(|i| (i, Vec::new())).collect();
    if files.is_empty() || count == 0 {
        return out;
    }
    let total: i128 = files.iter().map(|(_, d)| i128::from(d.0.max(0))).sum();
    for i in 0..count {
        let t = total * i128::from(2 * i + 1) / i128::from(2 * count);
        let mut start = 0i128;
        let mut placed = false;
        for (n, (_, d)) in files.iter().enumerate() {
            let d = i128::from(d.0.max(0));
            if t < start + d {
                out[n].1.push(Micros((t - start) as i64));
                placed = true;
                break;
            }
            start += d;
        }
        if !placed {
            let last = files.len() - 1;
            let d = files[last].1 .0.max(0);
            out[last].1.push(Micros(d));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use kv_media::{MediaError, MediaInfo, Rational, VideoFrame, VideoInfo};
    use std::path::Path;
    use std::sync::Mutex;

    fn files(durations_s: &[i64]) -> Vec<(PathBuf, Micros)> {
        durations_s
            .iter()
            .enumerate()
            .map(|(i, d)| (PathBuf::from(format!("c{i}.mp4")), Micros(d * 1_000_000)))
            .collect()
    }

    #[test]
    fn single_file_cells_are_centred() {
        let cells = cell_times(&files(&[4]), 4);
        assert_eq!(
            cells,
            vec![(
                0,
                vec![
                    Micros(500_000),
                    Micros(1_500_000),
                    Micros(2_500_000),
                    Micros(3_500_000)
                ]
            )]
        );
    }

    #[test]
    fn chaptered_cells_map_into_their_chapter() {
        // 10 s + 2 s, 6 cells at 1, 3, 5, 7, 9, 11 s.
        let cells = cell_times(&files(&[10, 2]), 6);
        assert_eq!(cells[0].1.len(), 5);
        assert_eq!(cells[1].1, vec![Micros(1_000_000)]);
    }

    /// A file opened for decoding, and the size asked for.
    type Opened = (PathBuf, Option<(u32, u32)>);

    /// Records which files were opened at which size; frames carry the
    /// requested time as pts.
    #[derive(Default)]
    struct FakeBackend {
        opened: Mutex<Vec<Opened>>,
    }

    struct FakeReader {
        info: VideoInfo,
        size: (u32, u32),
    }

    impl VideoReader for FakeReader {
        fn info(&self) -> &VideoInfo {
            &self.info
        }
        fn seek(&mut self, _t: Micros) -> kv_media::Result<()> {
            Ok(())
        }
        fn next_frame(&mut self) -> kv_media::Result<Option<VideoFrame>> {
            Ok(Some(VideoFrame::solid(
                Micros(0),
                self.size.0,
                self.size.1,
                [10, 20, 30, 255],
            )))
        }
        fn frame_at(&mut self, t: Micros) -> kv_media::Result<Option<VideoFrame>> {
            Ok(Some(VideoFrame::solid(
                t,
                self.size.0,
                self.size.1,
                [10, 20, 30, 255],
            )))
        }
    }

    fn video_info() -> VideoInfo {
        VideoInfo {
            codec: "fake".into(),
            width: 320,
            height: 240,
            fps: Rational { num: 30, den: 1 },
            bit_depth: 8,
            rotation: 0,
        }
    }

    impl MediaBackend for FakeBackend {
        fn probe(&self, path: &Path) -> kv_media::Result<MediaInfo> {
            if path.ends_with("novideo.mp4") {
                return Err(MediaError::NoVideoStream(path.to_path_buf()));
            }
            Ok(MediaInfo {
                duration: Micros(1_000_000),
                container: "fake".into(),
                video: Some(video_info()),
                audio: None,
                creation_time: None,
            })
        }
        fn open_video(
            &self,
            path: &Path,
            opts: &DecodeOptions,
        ) -> kv_media::Result<Box<dyn VideoReader>> {
            self.opened
                .lock()
                .unwrap()
                .push((path.to_path_buf(), opts.size));
            Ok(Box::new(FakeReader {
                info: video_info(),
                size: opts.size.unwrap(),
            }))
        }
    }

    #[test]
    fn media_filmstrips_decode_each_chapter_once_at_thumbnail_size() {
        let backend = Arc::new(FakeBackend::default());
        let gen = MediaFilmstrips(backend.clone());
        let thumbs = gen.generate(&files(&[10, 2]), 6, 90).unwrap();
        assert_eq!(thumbs.len(), 6);
        for jpeg in &thumbs {
            assert_eq!(&jpeg[..2], &[0xFF, 0xD8], "JPEG magic");
            let img = image_size(jpeg);
            assert_eq!(img, (120, 90));
        }
        let opened = backend.opened.lock().unwrap();
        assert_eq!(
            *opened,
            vec![
                (PathBuf::from("c0.mp4"), Some((120, 90))),
                (PathBuf::from("c1.mp4"), Some((120, 90)))
            ]
        );
    }

    #[test]
    fn media_filmstrips_fail_without_video() {
        let gen = MediaFilmstrips(Arc::new(FakeBackend::default()));
        let files = vec![(PathBuf::from("novideo.mp4"), Micros(1_000_000))];
        assert!(gen.generate(&files, 3, 90).is_err());
    }

    /// Width and height from the JPEG's SOF0 marker.
    fn image_size(jpeg: &[u8]) -> (u32, u32) {
        let mut i = 2;
        while i + 9 < jpeg.len() {
            assert_eq!(jpeg[i], 0xFF);
            let marker = jpeg[i + 1];
            let len = usize::from(u16::from_be_bytes([jpeg[i + 2], jpeg[i + 3]]));
            if marker == 0xC0 {
                let h = u16::from_be_bytes([jpeg[i + 5], jpeg[i + 6]]);
                let w = u16::from_be_bytes([jpeg[i + 7], jpeg[i + 8]]);
                return (u32::from(w), u32::from(h));
            }
            i += 2 + len;
        }
        panic!("no SOF0 marker");
    }
}
