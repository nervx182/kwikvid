//! [`VideoReader`] implemented with FFmpeg: decode, swscale to RGBA8,
//! optional display rotation.
//!
//! On macOS, H.264 and HEVC decode in hardware through FFmpeg's VideoToolbox
//! hwaccel when it is available: decoded frames are copied to system memory
//! (NV12 or P010) and go through the same swscale path as CPU frames. Any
//! hardware failure (no device, unsupported profile, an error mid-stream)
//! is logged once at `debug` and the reader carries on with CPU decode,
//! resuming after the last frame it produced. Elsewhere, and with
//! `KV_HWDECODE=0`, decode runs on the CPU with FFmpeg's frame threading.

// swscale's colour-space setup and FFmpeg's hwaccel have no `ffmpeg-next`
// wrapper, and the scaling context is not marked `Send`. Each `unsafe` block
// below says why it is sound.
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

/// Environment variable that turns hardware decode off: `KV_HWDECODE=0`
/// (also `false`, `off`, `no`) makes [`FfmpegBackend::open_video_reader`]
/// and [`MediaBackend::open_video`](crate::MediaBackend::open_video) decode
/// on the CPU, for comparisons and as an escape hatch.
pub const HWDECODE_ENV: &str = "KV_HWDECODE";

/// Which decoder [`FfmpegBackend::open_video_reader_with`] uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum HwDecode {
    /// Hardware decode where this platform has it (VideoToolbox on macOS,
    /// for H.264 and HEVC), else the CPU.
    #[default]
    Auto,
    /// Always decode on the CPU.
    Cpu,
}

impl HwDecode {
    /// [`HwDecode::Cpu`] when [`HWDECODE_ENV`] turns hardware decode off,
    /// else [`HwDecode::Auto`].
    pub fn from_env() -> Self {
        Self::from_env_value(std::env::var(HWDECODE_ENV).ok().as_deref())
    }

    fn from_env_value(value: Option<&str>) -> Self {
        match value.map(|v| v.trim().to_ascii_lowercase()) {
            Some(v) if matches!(v.as_str(), "0" | "false" | "off" | "no") => HwDecode::Cpu,
            _ => HwDecode::Auto,
        }
    }
}

impl FfmpegBackend {
    /// Like [`MediaBackend::open_video`](crate::MediaBackend::open_video),
    /// but returns the concrete reader (which also reports
    /// [`FfmpegVideoReader::seek_count`] and
    /// [`FfmpegVideoReader::hardware`]). Honours [`HWDECODE_ENV`].
    pub fn open_video_reader(
        &self,
        path: &Path,
        opts: &DecodeOptions,
    ) -> Result<FfmpegVideoReader> {
        self.open_video_reader_with(path, opts, HwDecode::from_env())
    }

    /// [`Self::open_video_reader`] with an explicit decoder choice (ignores
    /// [`HWDECODE_ENV`]).
    pub fn open_video_reader_with(
        &self,
        path: &Path,
        opts: &DecodeOptions,
        hwdecode: HwDecode,
    ) -> Result<FfmpegVideoReader> {
        FfmpegVideoReader::open(path, *opts, hwdecode)
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
    /// A hardware frame copied to system memory (then swapped into `raw`).
    downloaded: ff::frame::Video,
    scaled: ff::frame::Video,
    /// Frames are being decoded in hardware.
    hw: bool,
    /// After falling back to the CPU mid-stream, frames up to this pts were
    /// already produced and are dropped.
    skip_through: Option<Micros>,
    /// Container timestamp of the last seek (`None`: the start of the
    /// file), where a fallback with no frame produced yet resumes.
    last_seek: Option<i64>,
    /// Test hook: `hw` is simulated on a CPU decoder, which fails on this
    /// received frame (counted from 0 since open).
    #[cfg(test)]
    fail_hw_at: Option<u64>,
    #[cfg(test)]
    received: u64,
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
            .field("hardware", &self.hw)
            .finish_non_exhaustive()
    }
}

impl FfmpegVideoReader {
    fn open(path: &Path, opts: DecodeOptions, hwdecode: HwDecode) -> Result<Self> {
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
        let mut hw = false;
        let mut decoder = None;
        if hwdecode == HwDecode::Auto && videotoolbox::SUPPORTED {
            match new_decoder(stream.parameters(), true) {
                Ok(d) => {
                    decoder = Some(d);
                    hw = true;
                }
                Err(e) => tracing::debug!(
                    path = %path.display(),
                    "hardware decode unavailable ({e}); decoding on the CPU"
                ),
            }
        }
        let decoder = match decoder {
            Some(d) => d,
            None => new_decoder(stream.parameters(), false).map_err(decode_err)?,
        };

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

        let mut reader = FfmpegVideoReader {
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
            downloaded: ff::frame::Video::empty(),
            scaled: ff::frame::Video::empty(),
            hw,
            skip_through: None,
            last_seek: None,
            #[cfg(test)]
            fail_hw_at: None,
            #[cfg(test)]
            received: 0,
            queue: VecDeque::new(),
            current: None,
            at_start: true,
            eof_sent: false,
            last_pts: None,
            seeks: 0,
        };
        if reader.hw {
            // Decode the first frame now: hardware that cannot handle the
            // stream usually fails here, and the file is then reopened on the
            // CPU, so `hardware()` is accurate from the start.
            reader.peek_pts()?;
        }
        Ok(reader)
    }

    /// Whether frames are decoded in hardware (VideoToolbox). Settled when
    /// the reader is opened (the first frame is decoded then); turns false
    /// for good if the hardware fails later.
    pub fn hardware(&self) -> bool {
        self.hw
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
                Ok(()) => match self.take_frame()? {
                    Some(frame) => return Ok(Some(frame)),
                    None => continue,
                },
                Err(ff::Error::Eof) => return Ok(None),
                Err(ff::Error::Other { errno }) if errno == ff::error::EAGAIN => {}
                Err(e) if self.hw => {
                    self.fall_back_to_cpu(&e.to_string())?;
                    continue;
                }
                Err(e) => return Err(self.decode_err(e)),
            }
            if self.eof_sent {
                // The decoder asked for input after EOF; treat as the end.
                return Ok(None);
            }
            let sent = match packet.read(&mut self.ictx) {
                Ok(()) if packet.stream() == self.stream_index => self.decoder.send_packet(&packet),
                Ok(()) => Ok(()),
                Err(ff::Error::Eof) => {
                    self.eof_sent = true;
                    self.decoder.send_eof()
                }
                Err(e) => return Err(self.decode_err(e)),
            };
            match sent {
                Ok(()) => {}
                Err(e) if self.hw => self.fall_back_to_cpu(&e.to_string())?,
                Err(e) => return Err(self.decode_err(e)),
            }
        }
    }

    /// Turn the frame just received into `self.raw` into a [`VideoFrame`];
    /// `None` when it is dropped (already produced before a fallback, or the
    /// hardware failed on it).
    fn take_frame(&mut self) -> Result<Option<VideoFrame>> {
        #[cfg(test)]
        {
            let n = self.received;
            self.received += 1;
            if self.hw && self.fail_hw_at == Some(n) {
                self.fall_back_to_cpu("injected test failure")?;
                return Ok(None);
            }
        }
        let prev_pts = self.last_pts;
        let pts = match self.raw.timestamp().or_else(|| self.raw.pts()) {
            Some(ts) => self.ts_to_micros(ts),
            None => prev_pts.map_or(Micros(0), |p| Micros(p.0 + self.frame_duration.0)),
        };
        self.last_pts = Some(pts);
        if let Some(through) = self.skip_through {
            if pts <= through {
                return Ok(None);
            }
            self.skip_through = None;
        }
        if self.hw {
            if videotoolbox::is_hw_frame(&self.raw) {
                if let Err(e) = videotoolbox::download(&self.raw, &mut self.downloaded) {
                    // This frame was not produced: resume right before it.
                    self.last_pts = prev_pts;
                    self.fall_back_to_cpu(&e)?;
                    return Ok(None);
                }
                std::mem::swap(&mut self.raw, &mut self.downloaded);
            } else if videotoolbox::SUPPORTED && !self.simulated_hw() {
                // FFmpeg could not start the hwaccel for this stream and
                // let the decoder pick a software format instead.
                tracing::debug!(
                    path = %self.path.display(),
                    "hardware decode declined by the decoder; decoding on the CPU"
                );
                self.hw = false;
            }
        }
        self.convert(pts).map(Some)
    }

    /// Whether `hw` is a test's simulation (CPU frames standing in for
    /// hardware ones).
    #[cfg(test)]
    fn simulated_hw(&self) -> bool {
        self.fail_hw_at.is_some()
    }

    #[cfg(not(test))]
    fn simulated_hw(&self) -> bool {
        false
    }

    /// Replace the hardware decoder with a CPU one and resume after the last
    /// frame produced (or at the last seek point if none was).
    fn fall_back_to_cpu(&mut self, why: &str) -> Result<()> {
        tracing::debug!(
            path = %self.path.display(),
            "hardware decode failed ({why}); falling back to CPU decode"
        );
        self.hw = false;
        let params = self
            .ictx
            .stream(self.stream_index)
            .map(|s| s.parameters())
            .ok_or_else(|| self.decode_err("video stream vanished"))?;
        self.decoder = new_decoder(params, false).map_err(|e| self.decode_err(e))?;
        self.eof_sent = false;
        let res = match (self.last_pts, self.last_seek) {
            (Some(p), _) => {
                let ts = self.format_start_us.saturating_add(p.0);
                self.skip_through = Some(p);
                self.ictx.seek(ts, ..ts)
            }
            (None, Some(ts)) => self.ictx.seek(ts, ..ts),
            (None, None) => self.ictx.seek(self.format_start_us, ..),
        };
        self.last_pts = None;
        res.map_err(|e| self.decode_err(format!("seek after hardware fallback failed: {e}")))
    }

    /// Convert `self.raw`, displayed at `pts`, to an RGBA8 [`VideoFrame`].
    fn convert(&mut self, pts: Micros) -> Result<VideoFrame> {
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
                self.last_seek = None;
                self.ictx.seek(self.format_start_us, ..)
            } else {
                self.last_seek = Some(ts);
                self.ictx.seek(ts, ..ts)
            };
            self.skip_through = None;
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

/// Open a decoder for `params` with FFmpeg's frame threading, attaching the
/// VideoToolbox hwaccel when `hw` is set (macOS only).
fn new_decoder(
    params: ff::codec::Parameters,
    hw: bool,
) -> std::result::Result<ff::decoder::Video, String> {
    let mut ctx = ff::codec::Context::from_parameters(params).map_err(|e| e.to_string())?;
    ctx.set_threading(ff::threading::Config {
        kind: ff::threading::Type::Frame,
        ..Default::default()
    });
    if hw {
        videotoolbox::attach(&mut ctx)?;
    }
    ctx.decoder().video().map_err(|e| e.to_string())
}

/// FFmpeg's VideoToolbox hwaccel: device setup, format negotiation and the
/// copy of decoded frames to system memory.
#[cfg(target_os = "macos")]
mod videotoolbox {
    use std::ptr;

    use super::{ff, ffi};

    /// Hardware decode is implemented on this platform.
    pub(super) const SUPPORTED: bool = true;

    /// `kCMVideoCodecType_H264` ('avc1').
    const CODEC_TYPE_H264: u32 = u32::from_be_bytes(*b"avc1");
    /// `kCMVideoCodecType_HEVC` ('hvc1').
    const CODEC_TYPE_HEVC: u32 = u32::from_be_bytes(*b"hvc1");

    // VideoToolbox is a system framework (FFmpeg's hwaccel links it too), so
    // this adds no dependency.
    #[link(name = "VideoToolbox", kind = "framework")]
    extern "C" {
        /// `Boolean VTIsHardwareDecodeSupported(CMVideoCodecType)`.
        fn VTIsHardwareDecodeSupported(codec_type: u32) -> u8;
    }

    /// Create a VideoToolbox device and attach it, with a `get_format`
    /// callback that picks `AV_PIX_FMT_VIDEOTOOLBOX`, to the (not yet
    /// opened) decoder context. Only H.264 and HEVC are attempted.
    pub(super) fn attach(ctx: &mut ff::codec::Context) -> Result<(), String> {
        let id = ctx.id();
        let codec_type = match id {
            ff::codec::Id::H264 => CODEC_TYPE_H264,
            ff::codec::Id::HEVC => CODEC_TYPE_HEVC,
            _ => return Err(format!("no hardware decode for {id:?}")),
        };
        // Ask first: without a hardware decoder (a virtualised Mac, for
        // example) FFmpeg's hwaccel init fails and FFmpeg logs that at error
        // level on stderr before it falls back on its own.
        // SAFETY: a plain query of a system framework function, available
        // since macOS 10.13, with no pointers involved.
        if unsafe { VTIsHardwareDecodeSupported(codec_type) } == 0 {
            return Err(format!("this Mac has no hardware {id:?} decoder"));
        }
        // SAFETY: `avcodec_find_decoder` returns a pointer to a static codec
        // descriptor or null; `avcodec_get_hw_config` returns a pointer to a
        // static config of that codec or null past the last one.
        let has_config = unsafe {
            let codec = ffi::avcodec_find_decoder(ffi::AVCodecID::from(id));
            if codec.is_null() {
                return Err(format!("no decoder for {id:?}"));
            }
            let mut i = 0;
            loop {
                let config = ffi::avcodec_get_hw_config(codec, i);
                if config.is_null() {
                    break false;
                }
                if (*config).device_type == ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_VIDEOTOOLBOX
                    && (*config).methods & ffi::AV_CODEC_HW_CONFIG_METHOD_HW_DEVICE_CTX as i32 != 0
                {
                    break true;
                }
                i += 1;
            }
        };
        if !has_config {
            return Err(format!(
                "this FFmpeg has no VideoToolbox decoder for {id:?}"
            ));
        }
        let mut device: *mut ffi::AVBufferRef = ptr::null_mut();
        // SAFETY: `device` is a valid out pointer; on success FFmpeg stores a
        // new buffer reference in it, which we own.
        let ret = unsafe {
            ffi::av_hwdevice_ctx_create(
                &mut device,
                ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_VIDEOTOOLBOX,
                ptr::null(),
                ptr::null_mut(),
                0,
            )
        };
        if ret < 0 || device.is_null() {
            return Err(format!(
                "cannot create a VideoToolbox device: {}",
                ff::Error::from(ret)
            ));
        }
        // SAFETY: `ctx` owns a live, unopened AVCodecContext. It takes over
        // our reference to `device` (`avcodec_free_context` unrefs
        // `hw_device_ctx`), and `get_format` is a plain function.
        unsafe {
            let raw = ctx.as_mut_ptr();
            (*raw).hw_device_ctx = device;
            (*raw).get_format = Some(get_format);
        }
        Ok(())
    }

    /// Pick `AV_PIX_FMT_VIDEOTOOLBOX` when the decoder offers it, else let
    /// FFmpeg choose (a software format: CPU decode).
    unsafe extern "C" fn get_format(
        ctx: *mut ffi::AVCodecContext,
        formats: *const ffi::AVPixelFormat,
    ) -> ffi::AVPixelFormat {
        let wanted = ffi::AVPixelFormat::AV_PIX_FMT_VIDEOTOOLBOX as i32;
        let none = ffi::AVPixelFormat::AV_PIX_FMT_NONE as i32;
        // SAFETY: FFmpeg passes a list terminated by AV_PIX_FMT_NONE; it is
        // read as plain integers so no unknown value becomes a Rust enum.
        unsafe {
            let list = formats.cast::<i32>();
            let mut i = 0;
            loop {
                let f = *list.add(i);
                if f == none {
                    break;
                }
                if f == wanted {
                    return ffi::AVPixelFormat::AV_PIX_FMT_VIDEOTOOLBOX;
                }
                i += 1;
            }
            ffi::avcodec_default_get_format(ctx, formats)
        }
    }

    /// Whether `frame` is a VideoToolbox hardware frame.
    pub(super) fn is_hw_frame(frame: &ff::frame::Video) -> bool {
        // SAFETY: `frame` wraps a live AVFrame; `format` is a plain int.
        unsafe { (*frame.as_ptr()).format == ffi::AVPixelFormat::AV_PIX_FMT_VIDEOTOOLBOX as i32 }
    }

    /// Copy the hardware frame `src` to system memory in `dst` (NV12 or
    /// P010, the device's software format), with its timestamps and colour
    /// properties.
    pub(super) fn download(
        src: &ff::frame::Video,
        dst: &mut ff::frame::Video,
    ) -> Result<(), String> {
        // SAFETY: both wrap live AVFrames. `dst` is unreferenced first, so
        // `av_hwframe_transfer_data` allocates its buffers in the software
        // format; `av_frame_copy_props` only copies metadata.
        let ret = unsafe {
            ffi::av_frame_unref(dst.as_mut_ptr());
            let ret = ffi::av_hwframe_transfer_data(dst.as_mut_ptr(), src.as_ptr(), 0);
            if ret < 0 {
                ret
            } else {
                ffi::av_frame_copy_props(dst.as_mut_ptr(), src.as_ptr())
            }
        };
        if ret < 0 {
            return Err(format!(
                "cannot copy a hardware frame to memory: {}",
                ff::Error::from(ret)
            ));
        }
        Ok(())
    }
}

/// Stand-in off macOS: no hardware decode, nothing to set up.
#[cfg(not(target_os = "macos"))]
mod videotoolbox {
    use super::ff;

    /// Hardware decode is implemented on this platform.
    pub(super) const SUPPORTED: bool = false;

    pub(super) fn attach(_ctx: &mut ff::codec::Context) -> Result<(), String> {
        Err("hardware decode is only implemented on macOS".into())
    }

    pub(super) fn is_hw_frame(_frame: &ff::frame::Video) -> bool {
        false
    }

    pub(super) fn download(
        _src: &ff::frame::Video,
        _dst: &mut ff::frame::Video,
    ) -> Result<(), String> {
        Err("hardware decode is only implemented on macOS".into())
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
    use super::{FfmpegBackend, HwDecode, Micros, VideoReader};
    use crate::DecodeOptions;
    use kv_testkit::{read_frame_tag, tagged_clip, ClipSpec};

    #[test]
    fn env_switch_turns_hardware_decode_off() {
        for v in ["0", " 0 ", "false", "OFF", "no"] {
            assert_eq!(HwDecode::from_env_value(Some(v)), HwDecode::Cpu, "{v:?}");
        }
        for v in [None, Some("1"), Some(""), Some("auto"), Some("yes")] {
            assert_eq!(HwDecode::from_env_value(v), HwDecode::Auto, "{v:?}");
        }
    }

    /// A CPU reader on a 2 s, 30 fps tagged clip that pretends to decode in
    /// hardware and fails on received frame `fail_at`.
    fn failing_reader(fail_at: u64) -> super::FfmpegVideoReader {
        let path = tagged_clip(&ClipSpec {
            clip: 9,
            ..ClipSpec::default()
        });
        let mut r = FfmpegBackend::new()
            .unwrap()
            .open_video_reader_with(&path, &DecodeOptions::default(), HwDecode::Cpu)
            .unwrap();
        assert!(!r.hardware());
        r.hw = true;
        r.fail_hw_at = Some(fail_at);
        r
    }

    fn frame_index(f: &crate::VideoFrame) -> u16 {
        read_frame_tag(&f.data, f.width, f.height)
            .expect("tagged frame")
            .frame
    }

    #[test]
    fn hardware_failure_mid_stream_resumes_on_cpu_without_gaps() {
        for fail_at in [0, 1, 17, 59] {
            let mut r = failing_reader(fail_at);
            let mut n = 0u16;
            while let Some(f) = r.next_frame().unwrap() {
                assert_eq!(frame_index(&f), n, "fail at {fail_at}");
                assert_eq!(f.pts, Micros((i64::from(n) * 1_000_000 + 15) / 30));
                n += 1;
            }
            assert_eq!(n, 60, "fail at {fail_at}: every frame once");
            assert!(!r.hardware(), "fail at {fail_at}: now on the CPU");
            // The fallback re-decoded at least the failed frame.
            assert!(r.received > 60, "fail at {fail_at}: fell back");
        }
    }

    #[test]
    fn hardware_failure_right_after_a_seek_resumes_at_the_seek() {
        let mut r = failing_reader(u64::MAX);
        r.seek(Micros(1_000_000)).unwrap();
        let before = r.received;
        r.fail_hw_at = Some(before);
        // `seek` already decoded what it queued (frame 30 and the one after
        // it); the failure hits the next frame decoded.
        for want in 30..=33 {
            assert_eq!(frame_index(&r.next_frame().unwrap().unwrap()), want);
        }
        assert!(!r.hardware());
        assert!(r.received > before + 2, "fell back and re-decoded");

        let mut r = failing_reader(u64::MAX);
        r.fail_hw_at = Some(0);
        // `frame_at` walking forward over a failure.
        for i in 0..60u16 {
            let t = Micros((i64::from(i) * 1_000_000 + 15) / 30);
            assert_eq!(frame_index(&r.frame_at(t).unwrap().unwrap()), i);
        }
        assert_eq!(r.seek_count(), 0);
        assert!(!r.hardware());
        assert_eq!(r.received, 61, "frame 0 was decoded twice");
        let f = r.frame_at(Micros(1_500_000)).unwrap().unwrap();
        assert_eq!(frame_index(&f), 45);
    }

    #[test]
    fn hardware_failure_during_a_seek_lands_on_the_target() {
        let mut r = failing_reader(u64::MAX);
        r.next_frame().unwrap();
        // The next frame received is the first one after the container seek,
        // before any frame was produced there.
        r.fail_hw_at = Some(r.received);
        r.seek(Micros(1_400_000)).unwrap();
        assert!(!r.hardware());
        assert_eq!(frame_index(&r.next_frame().unwrap().unwrap()), 42);
        assert_eq!(frame_index(&r.next_frame().unwrap().unwrap()), 43);
    }

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
