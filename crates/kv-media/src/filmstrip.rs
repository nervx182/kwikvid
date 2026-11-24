//! Filmstrip thumbnails: `count` small frames evenly spaced over a clip, for
//! the media library (MVP item 2), plus JPEG encoding for caching and for
//! serving them to the webview (ADR 0007). No FFmpeg or platform code: this
//! works on any [`VideoReader`] and compiles without the `ffmpeg` feature.

use std::ops::RangeInclusive;

use image::codecs::jpeg::JpegEncoder;
use image::ExtendedColorType;
use kv_core::Micros;

use crate::{MediaError, PixelFormat, Result, VideoFrame, VideoInfo, VideoReader};

/// Allowed number of thumbnails per filmstrip.
const COUNT_RANGE: RangeInclusive<u32> = 1..=64;
/// Allowed thumbnail height in pixels.
const HEIGHT_RANGE: RangeInclusive<u32> = 16..=360;

/// How far before its cell centre a thumbnail may be when [`filmstrip`] has
/// to seek: the keyframe the seek lands on is used as is when it is at most
/// this much earlier, which saves decoding up to a GOP of frames per cell.
///
/// Accuracy trade-off: such a thumbnail shows a frame up to 0.5 s before
/// the cell centre (up to 30 frames at 60 fps). Cells reached by decoding
/// forward (short clips, or cells close together) are exact. Either way the
/// thumbnails stay in order and within their own cell for any clip whose
/// cells are wider than 0.5 s, which is all a filmstrip needs.
pub const FILMSTRIP_KEYFRAME_SNAP: Micros = Micros(500_000);

fn invalid(msg: String) -> MediaError {
    MediaError::InvalidThumbnail(msg)
}

/// Size `(width, height)` of the thumbnails [`filmstrip`] makes for a stream
/// with this `info` at `height` pixels: the display aspect ratio (coded size,
/// width and height swapped for a 90 or 270 degree rotation), width rounded
/// to the nearest even number (at least 2).
///
/// Open the reader with `DecodeOptions { size: Some(filmstrip_size(..)?),
/// apply_rotation: true }` so the decoder scales each frame once, instead of
/// converting full-size frames that [`filmstrip`] then shrinks.
///
/// Fails with [`MediaError::InvalidThumbnail`] unless `height` is in
/// 16..=360 and the stream has a non-zero size.
pub fn filmstrip_size(info: &VideoInfo, height: u32) -> Result<(u32, u32)> {
    if !HEIGHT_RANGE.contains(&height) {
        return Err(invalid(format!(
            "thumbnail height must be in {HEIGHT_RANGE:?}, got {height}"
        )));
    }
    let (w, h) = if info.rotation == 90 || info.rotation == 270 {
        (info.height, info.width)
    } else {
        (info.width, info.height)
    };
    if w == 0 || h == 0 {
        return Err(invalid(format!("stream has an empty size {w}x{h}")));
    }
    let (w, h, th) = (u64::from(w), u64::from(h), u64::from(height));
    // 2 * round(th * w / h / 2), rounding halves up.
    let half = (th * w + h) / (2 * h);
    let width = u32::try_from((2 * half).max(2))
        .map_err(|_| invalid(format!("thumbnail width overflows for {w}x{h}")))?;
    Ok((width, height))
}

/// `count` thumbnails `height` pixels high, evenly spaced over a clip that
/// lasts `duration` (use [`MediaInfo::duration`](crate::MediaInfo) from
/// [`MediaBackend::probe`](crate::MediaBackend::probe)).
///
/// Thumbnail `i` is the frame displayed at `duration * (i + 0.5) / count`,
/// the centre of its cell, fetched with [`VideoReader::frame_near`] and
/// [`FILMSTRIP_KEYFRAME_SNAP`] (see there for the accuracy trade-off). Cells
/// are visited in increasing time order, so the reader never seeks
/// backwards. Each thumbnail is [`filmstrip_size`] and keeps its frame's
/// `pts`; frames of another size are scaled with an area-average filter.
///
/// The reader must deliver upright frames (`apply_rotation`, the default).
///
/// Fails with [`MediaError::InvalidThumbnail`] unless `count` is in 1..=64,
/// `height` in 16..=360 and `duration` is not negative, or when the stream
/// yields no frame at all; decode errors are passed on.
pub fn filmstrip(
    reader: &mut dyn VideoReader,
    duration: Micros,
    count: u32,
    height: u32,
) -> Result<Vec<VideoFrame>> {
    if !COUNT_RANGE.contains(&count) {
        return Err(invalid(format!(
            "thumbnail count must be in {COUNT_RANGE:?}, got {count}"
        )));
    }
    if duration.0 < 0 {
        return Err(invalid(format!("negative clip duration {duration:?}")));
    }
    let (width, height) = filmstrip_size(reader.info(), height)?;

    let mut thumbs = Vec::with_capacity(count as usize);
    for i in 0..count {
        let t = cell_centre(duration, i, count);
        let frame = match reader.frame_near(t, FILMSTRIP_KEYFRAME_SNAP)? {
            Some(f) => f,
            // `t` is before the first frame (a stream starting late): use
            // the first frame instead.
            None => reader
                .next_frame()?
                .ok_or_else(|| invalid("the stream has no frames".to_owned()))?,
        };
        thumbs.push(resize(frame, width, height));
    }
    Ok(thumbs)
}

/// `duration * (2i + 1) / (2 count)`, rounded down.
fn cell_centre(duration: Micros, i: u32, count: u32) -> Micros {
    let t = i128::from(duration.0) * i128::from(2 * i + 1) / i128::from(2 * count);
    Micros(t as i64)
}

/// Scale an RGBA8 frame to `width x height` by averaging the source pixels
/// that each output pixel covers (at least one). Returns `frame` unchanged
/// when it already has that size.
fn resize(frame: VideoFrame, width: u32, height: u32) -> VideoFrame {
    if (frame.width, frame.height) == (width, height) {
        return frame;
    }
    let PixelFormat::Rgba8 = frame.format;
    let (sw, sh) = (frame.width as usize, frame.height as usize);
    let (dw, dh) = (width as usize, height as usize);
    if sw == 0 || sh == 0 {
        return VideoFrame::solid(frame.pts, width, height, [0, 0, 0, 255]);
    }
    let span = |i: usize, src: usize, dst: usize| {
        let a = i * src / dst;
        let b = ((i + 1) * src / dst).max(a + 1).min(src);
        a..b
    };
    let cols: Vec<_> = (0..dw).map(|x| span(x, sw, dw)).collect();
    let mut data = Vec::with_capacity(dw * dh * 4);
    for y in 0..dh {
        let rows = span(y, sh, dh);
        for xs in &cols {
            let mut sum = [0u32; 4];
            for sy in rows.clone() {
                let row = &frame.data[sy * sw * 4..(sy + 1) * sw * 4];
                for px in row[xs.start * 4..xs.end * 4].chunks_exact(4) {
                    for c in 0..4 {
                        sum[c] += u32::from(px[c]);
                    }
                }
            }
            let n = (rows.len() * xs.len()) as u32;
            data.extend(sum.iter().map(|&s| ((s + n / 2) / n) as u8));
        }
    }
    VideoFrame {
        pts: frame.pts,
        width,
        height,
        format: PixelFormat::Rgba8,
        data,
    }
}

/// Encode `frame` as a baseline JPEG at `quality` (clamped to 1..=100; 80 is
/// a good thumbnail default). Alpha is dropped.
///
/// Fails with [`MediaError::InvalidThumbnail`] for an empty frame or one
/// wider or higher than 65535 pixels (the JPEG limit), and with
/// [`MediaError::InvalidFrame`] when `data` does not match the frame size.
pub fn encode_jpeg(frame: &VideoFrame, quality: u8) -> Result<Vec<u8>> {
    let PixelFormat::Rgba8 = frame.format;
    let max = u32::from(u16::MAX);
    if frame.width == 0 || frame.height == 0 || frame.width > max || frame.height > max {
        return Err(invalid(format!(
            "cannot encode a {}x{} frame as JPEG",
            frame.width, frame.height
        )));
    }
    let expected = frame.width as usize * frame.height as usize * 4;
    if frame.data.len() != expected {
        return Err(MediaError::InvalidFrame(format!(
            "{}x{} RGBA8 needs {expected} bytes, got {}",
            frame.width,
            frame.height,
            frame.data.len()
        )));
    }
    let rgb: Vec<u8> = frame
        .data
        .chunks_exact(4)
        .flat_map(|px| [px[0], px[1], px[2]])
        .collect();
    let mut out = Vec::new();
    JpegEncoder::new_with_quality(&mut out, quality.clamp(1, 100))
        .encode(&rgb, frame.width, frame.height, ExtendedColorType::Rgb8)
        .map_err(|e| invalid(format!("JPEG encoding failed: {e}")))?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Rational;

    fn info(width: u32, height: u32, rotation: u16) -> VideoInfo {
        VideoInfo {
            codec: "test".into(),
            width,
            height,
            fps: Rational::new(30, 1),
            bit_depth: 8,
            rotation,
        }
    }

    /// A 10 fps clip of solid frames whose red channel is the frame index;
    /// keyframes every `gop` frames. Records every call.
    struct FakeReader {
        info: VideoInfo,
        frames: usize,
        gop: usize,
        calls: Vec<(&'static str, Micros)>,
    }

    impl FakeReader {
        fn new(frames: usize, gop: usize) -> Self {
            FakeReader {
                info: info(64, 48, 0),
                frames,
                gop,
                calls: Vec::new(),
            }
        }

        fn frame(&self, i: usize) -> VideoFrame {
            VideoFrame::solid(Micros(i as i64 * 100_000), 64, 48, [i as u8, 0, 0, 255])
        }

        fn index_at(&self, t: Micros) -> Option<usize> {
            (t.0 >= 0).then(|| ((t.0 / 100_000) as usize).min(self.frames - 1))
        }
    }

    impl VideoReader for FakeReader {
        fn info(&self) -> &VideoInfo {
            &self.info
        }
        fn seek(&mut self, _t: Micros) -> Result<()> {
            unimplemented!()
        }
        fn next_frame(&mut self) -> Result<Option<VideoFrame>> {
            unimplemented!()
        }
        fn frame_at(&mut self, t: Micros) -> Result<Option<VideoFrame>> {
            self.calls.push(("frame_at", t));
            Ok(self.index_at(t).map(|i| self.frame(i)))
        }
        fn frame_near(&mut self, t: Micros, tolerance: Micros) -> Result<Option<VideoFrame>> {
            self.calls.push(("frame_near", t));
            Ok(self.index_at(t).map(|i| {
                let key = i / self.gop * self.gop;
                let snap = t.0 - self.frame(key).pts.0 <= tolerance.0;
                self.frame(if snap { key } else { i })
            }))
        }
    }

    #[test]
    fn cells_are_centred_and_visited_in_increasing_order() {
        // 10 s, 100 frames, 8 cells of 1.25 s: centres at 0.625, 1.875, ...
        let mut r = FakeReader::new(100, 100);
        let thumbs = filmstrip(&mut r, Micros(10_000_000), 8, 24).unwrap();
        let times: Vec<i64> = r.calls.iter().map(|c| c.1 .0).collect();
        let want: Vec<i64> = (0..8).map(|i| 625_000 + i * 1_250_000).collect();
        assert_eq!(times, want);
        assert!(r.calls.iter().all(|c| c.0 == "frame_near"));
        assert_eq!(thumbs.len(), 8);
        for t in &thumbs {
            assert_eq!((t.width, t.height), (32, 24));
        }
    }

    #[test]
    fn keyframe_snap_is_used_within_half_a_second() {
        let mut r = FakeReader::new(100, 10);
        let thumbs = filmstrip(&mut r, Micros(10_000_000), 4, 24).unwrap();
        // Keyframes every 1 s. Centres 1.25, 3.75, 6.25, 8.75 s: the
        // keyframes at 1 and 6 s are within 0.5 s, those at 3 and 8 s not.
        let reds: Vec<u8> = thumbs.iter().map(|t| t.data[0]).collect();
        assert_eq!(reds, [10, 37, 60, 87]);
        assert_eq!(thumbs[0].pts, Micros(1_000_000));
    }

    #[test]
    fn default_frame_near_is_exact() {
        struct Exact(FakeReader);
        impl VideoReader for Exact {
            fn info(&self) -> &VideoInfo {
                self.0.info()
            }
            fn seek(&mut self, t: Micros) -> Result<()> {
                self.0.seek(t)
            }
            fn next_frame(&mut self) -> Result<Option<VideoFrame>> {
                self.0.next_frame()
            }
            fn frame_at(&mut self, t: Micros) -> Result<Option<VideoFrame>> {
                self.0.frame_at(t)
            }
        }
        let mut r = Exact(FakeReader::new(100, 10));
        let thumbs = filmstrip(&mut r, Micros(10_000_000), 4, 24).unwrap();
        let reds: Vec<u8> = thumbs.iter().map(|t| t.data[0]).collect();
        assert_eq!(reds, [12, 37, 62, 87]);
        assert!(r.0.calls.iter().all(|c| c.0 == "frame_at"));
    }

    #[test]
    fn rejects_out_of_range_arguments() {
        let mut r = FakeReader::new(10, 10);
        for (count, height) in [(0, 64), (65, 64), (8, 15), (8, 361)] {
            let err = filmstrip(&mut r, Micros(1_000_000), count, height).unwrap_err();
            assert!(
                matches!(err, MediaError::InvalidThumbnail(_)),
                "{count} x {height}: {err}"
            );
        }
        assert!(filmstrip(&mut r, Micros(-1), 8, 64).is_err());
        assert!(filmstrip(&mut r, Micros(1_000_000), 1, 16).is_ok());
        assert!(filmstrip(&mut r, Micros(1_000_000), 64, 360).is_ok());
        assert!(r.calls.len() == 65, "no decoding for rejected requests");
    }

    #[test]
    fn size_keeps_display_aspect_rounded_to_even() {
        assert_eq!(filmstrip_size(&info(1920, 1080, 0), 90).unwrap(), (160, 90));
        assert_eq!(filmstrip_size(&info(1920, 1080, 90), 90).unwrap(), (50, 90));
        assert_eq!(
            filmstrip_size(&info(1920, 1080, 270), 160).unwrap(),
            (90, 160)
        );
        assert_eq!(
            filmstrip_size(&info(1920, 1080, 180), 90).unwrap(),
            (160, 90)
        );
        // 4:3 at 25 px high is 33.3 px: nearest even is 34; 5.3K 16:9 at 100.
        assert_eq!(filmstrip_size(&info(640, 480, 0), 25).unwrap(), (34, 25));
        assert_eq!(
            filmstrip_size(&info(5312, 2988, 0), 100).unwrap(),
            (178, 100)
        );
        // Extremely tall streams still get a width of 2.
        assert_eq!(filmstrip_size(&info(2, 4000, 0), 16).unwrap(), (2, 16));
        assert!(filmstrip_size(&info(0, 1080, 0), 90).is_err());
    }

    #[test]
    fn resize_averages_areas() {
        // 4x2: left half black, right half white -> 2x1: black, white.
        let mut data = Vec::new();
        for _ in 0..2 {
            for v in [0u8, 0, 255, 255] {
                data.extend([v, v, v, 255]);
            }
        }
        let f = VideoFrame::new_rgba(Micros(7), 4, 2, data).unwrap();
        let small = resize(f.clone(), 2, 1);
        assert_eq!(small.pts, Micros(7));
        assert_eq!(small.data, [0, 0, 0, 255, 255, 255, 255, 255]);
        // 4 -> 1 averages to grey; upscaling repeats pixels.
        assert_eq!(resize(f.clone(), 1, 1).data, [128, 128, 128, 255]);
        let big = resize(f, 8, 4);
        assert_eq!((big.width, big.height, big.data.len()), (8, 4, 128));
    }

    #[test]
    fn jpeg_round_trips_size_and_colour() {
        let f = VideoFrame::solid(Micros(0), 48, 30, [200, 40, 90, 255]);
        let jpeg = encode_jpeg(&f, 85).unwrap();
        assert_eq!(&jpeg[..2], [0xFF, 0xD8], "SOI marker");
        let img = image::load_from_memory_with_format(&jpeg, image::ImageFormat::Jpeg)
            .unwrap()
            .to_rgb8();
        assert_eq!(img.dimensions(), (48, 30));
        for (got, want) in img.get_pixel(20, 15).0.iter().zip([200u8, 40, 90]) {
            assert!(got.abs_diff(want) <= 6, "{got} vs {want}");
        }
        // Lower quality makes a smaller file of the same size.
        let noisy = VideoFrame::new_rgba(
            Micros(0),
            64,
            64,
            (0..64 * 64 * 4).map(|i| (i * 37 % 251) as u8).collect(),
        )
        .unwrap();
        assert!(encode_jpeg(&noisy, 20).unwrap().len() < encode_jpeg(&noisy, 95).unwrap().len());
    }

    #[test]
    fn jpeg_rejects_bad_frames() {
        let empty = VideoFrame::solid(Micros(0), 0, 0, [0; 4]);
        assert!(matches!(
            encode_jpeg(&empty, 80),
            Err(MediaError::InvalidThumbnail(_))
        ));
        let mut short = VideoFrame::solid(Micros(0), 4, 4, [0; 4]);
        short.data.pop();
        assert!(matches!(
            encode_jpeg(&short, 80),
            Err(MediaError::InvalidFrame(_))
        ));
    }
}
