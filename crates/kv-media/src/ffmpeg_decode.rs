//! [`VideoReader`] implemented with FFmpeg: CPU decode, swscale to RGBA8,
//! optional display rotation.
//!
//! Decode runs on the CPU with FFmpeg's frame threading; hardware decode
//! (VideoToolbox) is a later performance item.

// swscale's colour-space setup has no `ffmpeg-next` wrapper, and its scaling
// context is not marked `Send`. Each `unsafe` block below says why it is
// sound.
#![allow(unsafe_code)]

use std::collections::VecDeque;
use std::path::{Path, PathBuf};

use ff::ffi;
use ffmpeg_next as ff;
use kv_core::Micros;

use crate::ffmpeg::video_info;
use crate::{DecodeOptions, FfmpegBackend, MediaError, Result, VideoFrame, VideoInfo, VideoReader};

/// `frame_at` decodes forward instead of seeking when the target is at most
/// this far ahead of the next frame (guess: about one GoPro GOP).
const FORWARD_WINDOW: Micros = Micros(2_000_000);

/// How many times a seek backs off further when the keyframe it landed on is
/// still after the target.
const SEEK_RETRIES: u32 = 6;

impl FfmpegBackend {
    /// Like [`MediaBackend::open_video`](crate::MediaBackend::open_video),
    /// but returns the concrete reader (which also reports
    /// [`FfmpegVideoReader::seek_count`]).
    pub fn open_video_reader(
        &self,
        path: &Path,
        opts: &DecodeOptions,
    ) -> Result<FfmpegVideoReader> {
        FfmpegVideoReader::open(path, *opts)
    }
}

/// FFmpeg-based [`VideoReader`]. Obtain one with
/// [`FfmpegBackend::open_video_reader`] or
/// [`MediaBackend::open_video`](crate::MediaBackend::open_video).
pub struct FfmpegVideoReader {
    path: PathBuf,
    ictx: ff::format::context::Input,
    stream_index: usize,
    decoder: ff::decoder::Video,
    time_base: ff::Rational,
    /// Stream start time in stream time base units.
    start_pts: i64,
    /// Container start time in microseconds (`AV_TIME_BASE`), the origin of
    /// `avformat_seek_file` timestamps.
    format_start_us: i64,
    /// Expected frame duration, for frames without a timestamp.
    frame_duration: Micros,
    info: VideoInfo,
    opts: DecodeOptions,
    scaler: Option<Scaler>,
    raw: ff::frame::Video,
    scaled: ff::frame::Video,
    /// Decoded frames not yet returned, in presentation order.
    queue: VecDeque<VideoFrame>,
    /// The frame `frame_at` returned last; frames before `queue`'s front.
    current: Option<VideoFrame>,
    /// No frame has been consumed or skipped since the file was opened, so
    /// nothing precedes `queue`'s front.
    at_start: bool,
    eof_sent: bool,
    last_pts: Option<Micros>,
    seeks: u64,
}

/// swscale context plus the input it was built for.
struct Scaler {
    ctx: ff::software::scaling::Context,
    key: (ff::format::Pixel, u32, u32, u32, u32),
}

// SAFETY: an `SwsContext` has no thread affinity; it is owned by exactly one
// reader and only used through `&mut self`, so moving it between threads is
// fine (`ffmpeg-next` just does not declare it).
unsafe impl Send for Scaler {}

impl std::fmt::Debug for FfmpegVideoReader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FfmpegVideoReader")
            .field("path", &self.path)
            .field("info", &self.info)
            .field("opts", &self.opts)
            .finish_non_exhaustive()
    }
}

impl FfmpegVideoReader {
    fn open(path: &Path, opts: DecodeOptions) -> Result<Self> {
        let ictx = ff::format::input(path).map_err(|e| MediaError::Open {
            path: path.to_path_buf(),
            message: e.to_string(),
        })?;
        let decode_err = |message: String| MediaError::Decode {
            path: path.to_path_buf(),
            message,
        };
        let stream = ictx
            .streams()
            .best(ff::media::Type::Video)
            .ok_or_else(|| MediaError::NoVideoStream(path.to_path_buf()))?;
        let info = video_info(&stream).map_err(decode_err)?;
        let mut ctx = ff::codec::Context::from_parameters(stream.parameters())
            .map_err(|e| decode_err(e.to_string()))?;
        ctx.set_threading(ff::threading::Config {
            kind: ff::threading::Type::Frame,
            ..Default::default()
        });
        let decoder = ctx
            .decoder()
            .video()
            .map_err(|e| decode_err(e.to_string()))?;

        let time_base = stream.time_base();
        if time_base.numerator() <= 0 || time_base.denominator() <= 0 {
            return Err(decode_err(format!("invalid time base {time_base}")));
        }
        let start_pts = Some(stream.start_time())
            .filter(|&s| s != ffi::AV_NOPTS_VALUE)
            .unwrap_or(0);
        // SAFETY: `ictx` owns a valid AVFormatContext for the read.
        let format_start_us = Some(unsafe { (*ictx.as_ptr()).start_time })
            .filter(|&s| s != ffi::AV_NOPTS_VALUE)
            .unwrap_or(0);
        let fps = info.fps.as_f64();
        let frame_duration = Micros(if fps > 0.0 {
            (1_000_000.0 / fps).round() as i64
        } else {
            33_333
        });
        let stream_index = stream.index();

        Ok(FfmpegVideoReader {
            path: path.to_path_buf(),
            ictx,
            stream_index,
            decoder,
            time_base,
            start_pts,
            format_start_us,
            frame_duration,
            info,
            opts,
            scaler: None,
            raw: ff::frame::Video::empty(),
            scaled: ff::frame::Video::empty(),
            queue: VecDeque::new(),
            current: None,
            at_start: true,
            eof_sent: false,
            last_pts: None,
            seeks: 0,
        })
    }

    /// How many container seeks this reader has done (for tests and
    /// diagnostics: sequential `frame_at` calls must not add any).
    pub fn seek_count(&self) -> u64 {
        self.seeks
    }

    fn decode_err(&self, e: impl std::fmt::Display) -> MediaError {
        MediaError::Decode {
            path: self.path.clone(),
            message: e.to_string(),
        }
    }

    /// Stream timestamp to `Micros` relative to the stream start, rounded to
    /// the nearest microsecond.
    fn ts_to_micros(&self, ts: i64) -> Micros {
        let num = i128::from(ts) - i128::from(self.start_pts);
        let n = num * i128::from(self.time_base.numerator()) * 1_000_000;
        let d = i128::from(self.time_base.denominator());
        let q = n.div_euclid(d);
        let r = n.rem_euclid(d);
        Micros((q + i128::from(2 * r >= d)) as i64)
    }

    /// Decode the next frame from the file, `None` at the end.
    fn decode_next(&mut self) -> Result<Option<VideoFrame>> {
        let mut packet = ff::Packet::empty();
        loop {
            match self.decoder.receive_frame(&mut self.raw) {
                Ok(()) => return self.convert().map(Some),
                Err(ff::Error::Eof) => return Ok(None),
                Err(ff::Error::Other { errno }) if errno == ff::error::EAGAIN => {}
                Err(e) => return Err(self.decode_err(e)),
            }
            if self.eof_sent {
                // The decoder asked for input after EOF; treat as the end.
                return Ok(None);
            }
            match packet.read(&mut self.ictx) {
                Ok(()) => {
                    if packet.stream() == self.stream_index {
                        self.decoder
                            .send_packet(&packet)
                            .map_err(|e| self.decode_err(e))?;
                    }
                }
                Err(ff::Error::Eof) => {
                    self.decoder.send_eof().map_err(|e| self.decode_err(e))?;
                    self.eof_sent = true;
                }
                Err(e) => return Err(self.decode_err(e)),
            }
        }
    }

    /// Convert `self.raw` to an RGBA8 [`VideoFrame`].
    fn convert(&mut self) -> Result<VideoFrame> {
        let pts = match self.raw.timestamp().or_else(|| self.raw.pts()) {
            Some(ts) => self.ts_to_micros(ts),
            None => self
                .last_pts
                .map_or(Micros(0), |p| Micros(p.0 + self.frame_duration.0)),
        };
        self.last_pts = Some(pts);

        let (src_w, src_h) = (self.raw.width(), self.raw.height());
        if src_w == 0 || src_h == 0 {
            return Err(self.decode_err("decoder returned an empty frame"));
        }
        let rotation = if self.opts.apply_rotation {
            self.info.rotation
        } else {
            0
        };
        let quarter = rotation == 90 || rotation == 270;
        // Size after scaling, before rotation.
        let (scale_w, scale_h) = match self.opts.size {
            None => (src_w, src_h),
            Some((w, h)) if quarter => (h, w),
            Some((w, h)) => (w, h),
        };
        if scale_w == 0 || scale_h == 0 {
            return Err(self.decode_err(format!("invalid output size {scale_w}x{scale_h}")));
        }

        let key = (self.raw.format(), src_w, src_h, scale_w, scale_h);
        if self.scaler.as_ref().is_none_or(|s| s.key != key) {
            self.scaler = Some(self.new_scaler(key)?);
            self.scaled = ff::frame::Video::empty();
        }
        let scaler = self.scaler.as_mut().expect("scaler just created");
        scaler
            .ctx
            .run(&self.raw, &mut self.scaled)
            .map_err(|e| MediaError::Decode {
                path: self.path.clone(),
                message: format!("scaling failed: {e}"),
            })?;

        let row = scale_w as usize * 4;
        let stride = self.scaled.stride(0);
        let plane = self.scaled.data(0);
        let mut rgba = Vec::with_capacity(row * scale_h as usize);
        for y in 0..scale_h as usize {
            rgba.extend_from_slice(&plane[y * stride..y * stride + row]);
        }
        let (rgba, w, h) = rotate_rgba(rgba, scale_w, scale_h, rotation);
        VideoFrame::new_rgba(pts, w, h, rgba)
    }

    fn new_scaler(&self, key: (ff::format::Pixel, u32, u32, u32, u32)) -> Result<Scaler> {
        let (format, src_w, src_h, dst_w, dst_h) = key;
        let mut ctx = ff::software::scaling::Context::get(
            format,
            src_w,
            src_h,
            ff::format::Pixel::RGBA,
            dst_w,
            dst_h,
            ff::software::scaling::Flags::BILINEAR | ff::software::scaling::Flags::ACCURATE_RND,
        )
        .map_err(|e| self.decode_err(format!("cannot convert {format:?} to RGBA: {e}")))?;

        // swscale assumes BT.601 for YUV input unless told otherwise; use the
        // stream's matrix and range (untagged HD is taken as BT.709).
        let space = match self.raw.color_space() {
            ff::color::Space::Unspecified | ff::color::Space::Reserved => {
                if src_h >= 720 {
                    ff::color::Space::BT709
                } else {
                    ff::color::Space::BT470BG
                }
            }
            s => s,
        };
        let space = ffi::AVColorSpace::from(space) as i32;
        let full_range = self.raw.color_range() == ff::color::Range::JPEG
            || matches!(
                format,
                ff::format::Pixel::YUVJ420P
                    | ff::format::Pixel::YUVJ422P
                    | ff::format::Pixel::YUVJ444P
            );
        // SAFETY: `ctx` owns a live SwsContext; `sws_getCoefficients` returns
        // a pointer into swscale's static coefficient table (a default one for
        // unknown spaces). For RGB input the call fails harmlessly (returns
        // -1) and the context keeps its defaults.
        unsafe {
            let coeffs = ffi::sws_getCoefficients(space);
            let rgb = ffi::sws_getCoefficients(ffi::SWS_CS_DEFAULT as _);
            ffi::sws_setColorspaceDetails(
                ctx.as_mut_ptr(),
                coeffs,
                i32::from(full_range),
                rgb,
                1,
                0,
                1 << 16,
                1 << 16,
            );
        }
        Ok(Scaler { ctx, key })
    }

    /// Make sure `queue` holds at least one frame unless the stream is over;
    /// returns the pts of its front.
    fn peek_pts(&mut self) -> Result<Option<Micros>> {
        if self.queue.is_empty() {
            if let Some(f) = self.decode_next()? {
                self.queue.push_back(f);
            }
        }
        Ok(self.queue.front().map(|f| f.pts))
    }

    /// Seek the container to the keyframe at or before `t` and reset the
    /// decoder; afterwards `queue` holds the first decoded frame, whose pts
    /// is `<= t` unless `t` is before the stream's first frame.
    fn seek_to(&mut self, t: Micros) -> Result<()> {
        self.seeks += 1;
        let mut backoff = 0i64;
        for attempt in 0..=SEEK_RETRIES {
            let target = t.0.saturating_sub(backoff).max(0);
            let ts = self.format_start_us.saturating_add(target);
            // Keyframe at or before `ts`; at the start of the file seek to
            // its very beginning.
            let res = if target == 0 {
                self.ictx.seek(self.format_start_us, ..)
            } else {
                self.ictx.seek(ts, ..ts)
            };
            res.map_err(|e| self.decode_err(format!("seek to {t:?} failed: {e}")))?;
            self.decoder.flush();
            self.queue.clear();
            self.current = None;
            self.eof_sent = false;
            self.last_pts = None;
            self.at_start = target == 0;

            let first = self.decode_next()?;
            let too_late = first.as_ref().is_some_and(|f| f.pts > t);
            if let Some(f) = first {
                self.queue.push_back(f);
            }
            if !too_late || target == 0 || attempt == SEEK_RETRIES {
                return Ok(());
            }
            backoff = if backoff == 0 { 1_000_000 } else { backoff * 2 };
        }
        Ok(())
    }

    /// Whether reaching the frame displayed at `t` needs a seek: it does
    /// unless that frame can be reached by decoding forward (nothing before
    /// `t` has been dropped) and is at most [`FORWARD_WINDOW`] ahead.
    fn needs_seek(&mut self, t: Micros) -> Result<bool> {
        let next = self.peek_pts()?;
        let reachable = match &self.current {
            Some(cur) => cur.pts <= t,
            None => self.at_start || next.is_some_and(|n| n <= t),
        };
        let near = next.is_none_or(|n| t.0.saturating_sub(n.0) <= FORWARD_WINDOW.0);
        Ok(!(reachable && near))
    }

    /// Consume frames with pts `<= t` into `current`.
    fn advance_to(&mut self, t: Micros) -> Result<()> {
        while let Some(pts) = self.peek_pts()? {
            if pts > t {
                break;
            }
            self.current = self.queue.pop_front();
            self.at_start = false;
        }
        Ok(())
    }
}

impl VideoReader for FfmpegVideoReader {
    fn info(&self) -> &VideoInfo {
        &self.info
    }

    fn seek(&mut self, t: Micros) -> Result<()> {
        self.seek_to(t)?;
        self.advance_to(t)?;
        if let Some(cur) = self.current.take() {
            self.queue.push_front(cur);
        }
        Ok(())
    }

    fn next_frame(&mut self) -> Result<Option<VideoFrame>> {
        self.peek_pts()?;
        let frame = self.queue.pop_front();
        // Not kept as `current`, to avoid copying every frame; a later
        // `frame_at` that needs it seeks back.
        self.current = None;
        self.at_start = false;
        Ok(frame)
    }

    fn frame_at(&mut self, t: Micros) -> Result<Option<VideoFrame>> {
        let next = self.peek_pts()?;
        if let Some(cur) = &self.current {
            if cur.pts <= t && next.is_none_or(|n| n > t) {
                return Ok(Some(cur.clone()));
            }
        }
        if self.needs_seek(t)? {
            self.seek_to(t)?;
        }
        self.advance_to(t)?;
        Ok(self.current.clone())
    }

    fn frame_near(&mut self, t: Micros, tolerance: Micros) -> Result<Option<VideoFrame>> {
        if !self.needs_seek(t)? {
            return self.frame_at(t);
        }
        self.seek_to(t)?;
        // `seek_to` leaves the frame it landed on (the keyframe) in front.
        if let Some(key) = self.queue.front() {
            if key.pts <= t && t.0 - key.pts.0 <= tolerance.0 {
                self.current = self.queue.pop_front();
                self.at_start = false;
                return Ok(self.current.clone());
            }
        }
        self.advance_to(t)?;
        Ok(self.current.clone())
    }
}

/// Rotate tightly packed RGBA8 clockwise by 0, 90, 180 or 270 degrees.
/// Returns the pixels and the new width and height.
fn rotate_rgba(src: Vec<u8>, w: u32, h: u32, degrees: u16) -> (Vec<u8>, u32, u32) {
    let (wu, hu) = (w as usize, h as usize);
    let px = |x: usize, y: usize| {
        let o = (y * wu + x) * 4;
        [src[o], src[o + 1], src[o + 2], src[o + 3]]
    };
    match degrees {
        90 => {
            // Destination is h x w; its (x, y) shows source (y, h - 1 - x).
            let mut dst = Vec::with_capacity(src.len());
            for y in 0..wu {
                for x in 0..hu {
                    dst.extend_from_slice(&px(y, hu - 1 - x));
                }
            }
            (dst, h, w)
        }
        180 => {
            let mut dst = Vec::with_capacity(src.len());
            for y in 0..hu {
                for x in 0..wu {
                    dst.extend_from_slice(&px(wu - 1 - x, hu - 1 - y));
                }
            }
            (dst, w, h)
        }
        270 => {
            // Destination is h x w; its (x, y) shows source (w - 1 - y, x).
            let mut dst = Vec::with_capacity(src.len());
            for y in 0..wu {
                for x in 0..hu {
                    dst.extend_from_slice(&px(wu - 1 - y, x));
                }
            }
            (dst, h, w)
        }
        _ => (src, w, h),
    }
}

#[cfg(test)]
mod tests {
    use super::rotate_rgba;

    /// A 3x2 image whose pixel (x, y) is `[x, y, 0, 255]`.
    fn image() -> Vec<u8> {
        let mut v = Vec::new();
        for y in 0..2u8 {
            for x in 0..3u8 {
                v.extend_from_slice(&[x, y, 0, 255]);
            }
        }
        v
    }

    fn at(img: &[u8], w: u32, x: u32, y: u32) -> (u8, u8) {
        let o = ((y * w + x) * 4) as usize;
        (img[o], img[o + 1])
    }

    #[test]
    fn rotate_90_moves_top_left_to_top_right() {
        let (r, w, h) = rotate_rgba(image(), 3, 2, 90);
        assert_eq!((w, h), (2, 3));
        assert_eq!(at(&r, w, 1, 0), (0, 0));
        assert_eq!(at(&r, w, 0, 0), (0, 1));
        assert_eq!(at(&r, w, 0, 2), (2, 1));
    }

    #[test]
    fn rotate_180_and_270() {
        let (r, w, h) = rotate_rgba(image(), 3, 2, 180);
        assert_eq!((w, h), (3, 2));
        assert_eq!(at(&r, w, 0, 0), (2, 1));
        assert_eq!(at(&r, w, 2, 1), (0, 0));

        let (r, w, h) = rotate_rgba(image(), 3, 2, 270);
        assert_eq!((w, h), (2, 3));
        assert_eq!(at(&r, w, 0, 2), (0, 0));
        assert_eq!(at(&r, w, 1, 0), (2, 1));
    }

    #[test]
    fn rotate_four_quarters_is_identity() {
        let mut img = (image(), 3, 2);
        for _ in 0..4 {
            img = rotate_rgba(img.0, img.1, img.2, 90);
        }
        assert_eq!(img, (image(), 3, 2));
    }

    #[test]
    fn rotate_0_is_identity() {
        assert_eq!(rotate_rgba(image(), 3, 2, 0), (image(), 3, 2));
    }
}
