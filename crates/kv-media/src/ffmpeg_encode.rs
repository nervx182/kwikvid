//! [`Encoder`] implemented with FFmpeg: RGBA frames and interleaved `f32`
//! PCM in, an MP4 with one H.264 and one AAC stream out.
//!
//! H.264 encoder order: `h264_videotoolbox` (macOS only), `libopenh264`,
//! `libx264` (GPL: development and CI only). See this crate's `README.md`.

// `ffmpeg-next` has no safe API for swscale colour details, frame durations,
// audio channel layouts or audio frame buffers, and its scaling context is not
// `Send`, so this module calls those FFmpeg functions directly. Each `unsafe`
// block only touches pointers owned by a live `ffmpeg-next` wrapper or by
// [`Scaler`], within the sizes FFmpeg allocated.
#![allow(unsafe_code)]

use std::path::{Path, PathBuf};
use std::ptr;

use ff::ffi;
use ffmpeg_next as ff;

use crate::{
    Encoder, ExportSettings, FfmpegBackend, MediaError, PixelFormat, Result, VideoFrame,
    VideoWriter,
};

/// Env var that turns a VideoToolbox fallback into an error.
pub const REQUIRE_VIDEOTOOLBOX_ENV: &str = "KV_REQUIRE_VIDEOTOOLBOX";

/// AAC bitrate in bits per second (guess: transparent for music at 48 kHz).
const AUDIO_BITRATE: usize = 192_000;

const VIDEOTOOLBOX: &str = "h264_videotoolbox";

/// H.264 encoders to try, in order, with the pixel format each is fed.
fn h264_candidates() -> Vec<(&'static str, ff::format::Pixel)> {
    let mut c = Vec::new();
    if cfg!(target_os = "macos") {
        c.push((VIDEOTOOLBOX, ff::format::Pixel::NV12));
    }
    c.push(("libopenh264", ff::format::Pixel::YUV420P));
    c.push(("libx264", ff::format::Pixel::YUV420P));
    c
}

fn require_videotoolbox_from_env() -> bool {
    std::env::var(REQUIRE_VIDEOTOOLBOX_ENV).is_ok_and(|v| v == "1")
}

impl Encoder for FfmpegBackend {
    fn create(&self, path: &Path, settings: &ExportSettings) -> Result<Box<dyn VideoWriter>> {
        settings.validate()?;
        let writer = FfmpegWriter::open(path, settings, require_videotoolbox_from_env())?;
        Ok(Box::new(writer))
    }
}

/// An open H.264 encoder and what it needs to be fed.
struct OpenedVideo {
    name: String,
    encoder: ff::encoder::Video,
    pixel: ff::format::Pixel,
}

/// Open the first usable H.264 encoder from [`h264_candidates`].
fn open_h264(
    settings: &ExportSettings,
    global_header: bool,
    require_videotoolbox: bool,
) -> std::result::Result<OpenedVideo, String> {
    let candidates = h264_candidates();
    if require_videotoolbox && !candidates.iter().any(|(n, _)| *n == VIDEOTOOLBOX) {
        return Err(format!(
            "{REQUIRE_VIDEOTOOLBOX_ENV}=1 but VideoToolbox is only available on macOS"
        ));
    }
    let mut tried = Vec::new();
    for (name, pixel) in candidates {
        let result = open_h264_encoder(name, pixel, settings, global_header);
        match result {
            Ok(encoder) => {
                if name == "libx264" {
                    tracing::warn!(
                        "encoding with libx264, which is GPL: acceptable for development \
                         and CI only, never in a shipped build (ADR 0005)"
                    );
                }
                return Ok(OpenedVideo {
                    name: name.to_owned(),
                    encoder,
                    pixel,
                });
            }
            Err(e) if name == VIDEOTOOLBOX => {
                if require_videotoolbox {
                    return Err(format!(
                        "{VIDEOTOOLBOX} failed ({e}) and {REQUIRE_VIDEOTOOLBOX_ENV}=1 forbids \
                         a software fallback"
                    ));
                }
                tracing::warn!(
                    "{VIDEOTOOLBOX} unavailable ({e}); falling back to a software H.264 encoder"
                );
                tried.push(format!("{name}: {e}"));
            }
            Err(e) => tried.push(format!("{name}: {e}")),
        }
    }
    Err(format!("no usable H.264 encoder ({})", tried.join("; ")))
}

fn open_h264_encoder(
    name: &str,
    pixel: ff::format::Pixel,
    settings: &ExportSettings,
    global_header: bool,
) -> std::result::Result<ff::encoder::Video, String> {
    let codec = ff::encoder::find_by_name(name).ok_or("not built into this FFmpeg")?;
    let mut enc = ff::codec::Context::new_with_codec(codec)
        .encoder()
        .video()
        .map_err(|e| e.to_string())?;
    enc.set_width(settings.width);
    enc.set_height(settings.height);
    enc.set_format(pixel);
    enc.set_time_base(video_time_base(settings));
    enc.set_frame_rate(Some(ff::Rational::new(settings.fps.num, settings.fps.den)));
    enc.set_bit_rate(settings.video_bitrate as usize);
    // One keyframe per second (rounded up) keeps seeking in players snappy.
    enc.set_gop(settings.fps.num.div_euclid(settings.fps.den).max(1) as u32);
    enc.set_colorspace(ff::color::Space::BT709);
    enc.set_color_range(ff::color::Range::MPEG);
    enc.set_color_primaries(ff::color::Primaries::BT709);
    enc.set_color_transfer_characteristic(ff::color::TransferCharacteristic::BT709);
    if global_header {
        enc.set_flags(ff::codec::Flags::GLOBAL_HEADER);
    }
    let mut opts = ff::Dictionary::new();
    match name {
        "libx264" => opts.set("preset", "veryfast"),
        // Hardware only: a software VideoToolbox session is no better than
        // our own software fallback, and hides a missing hardware encoder.
        VIDEOTOOLBOX => opts.set("allow_sw", "0"),
        _ => {}
    }
    enc.open_with(opts).map_err(|e| e.to_string())
}

/// Encoder time base: one tick per frame.
fn video_time_base(settings: &ExportSettings) -> ff::Rational {
    ff::Rational::new(settings.fps.den, settings.fps.num)
}

/// `pts` in encoder ticks (frames), rounded to the nearest frame.
fn frame_index(settings: &ExportSettings, pts: kv_core::Micros) -> i64 {
    let (num, den) = (i128::from(settings.fps.num), i128::from(settings.fps.den));
    let scale = den * 1_000_000;
    let x = i128::from(pts.0) * num;
    (2 * x + scale).div_euclid(2 * scale) as i64
}

/// An owned swscale context. `ffmpeg-next`'s wrapper is not `Send`; ours is,
/// because the context is only used through `&mut self`.
struct Scaler(*mut ffi::SwsContext);

// SAFETY: the context is owned exclusively and has no thread affinity.
unsafe impl Send for Scaler {}

impl Scaler {
    /// RGBA (full range) to `dst` at the same size, BT.709 limited range.
    fn new(width: u32, height: u32, dst: ff::format::Pixel) -> std::result::Result<Self, String> {
        let (w, h) = (width as i32, height as i32);
        // SAFETY: plain constructor calls; null filters and params are allowed.
        unsafe {
            let ctx = ffi::sws_getContext(
                w,
                h,
                ffi::AVPixelFormat::AV_PIX_FMT_RGBA,
                w,
                h,
                dst.into(),
                (ff::software::scaling::Flags::BILINEAR
                    | ff::software::scaling::Flags::ACCURATE_RND)
                    .bits() as _,
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null(),
            );
            if ctx.is_null() {
                return Err(format!("swscale cannot convert RGBA to {dst:?}"));
            }
            let scaler = Scaler(ctx);
            let bt709 = ffi::sws_getCoefficients(ffi::SWS_CS_ITU709 as _);
            let ret = ffi::sws_setColorspaceDetails(
                ctx,
                bt709,
                1, // source: full-range RGB
                bt709,
                0, // destination: limited ("MPEG") range
                0,
                1 << 16,
                1 << 16,
            );
            if ret < 0 {
                return Err("swscale rejected BT.709 colour details".into());
            }
            Ok(scaler)
        }
    }

    fn convert(&mut self, src: &VideoFrame, dst: &mut ff::frame::Video) -> Result<(), String> {
        let stride = [src.stride() as i32, 0, 0, 0];
        let planes = [src.data.as_ptr(), ptr::null(), ptr::null(), ptr::null()];
        // SAFETY: `dst` owns a refcounted frame of the scaler's output size and
        // format (made writable first); `src` holds `stride * height` bytes,
        // checked by the caller.
        let ret = unsafe {
            let d = dst.as_mut_ptr();
            if ffi::av_frame_make_writable(d) < 0 {
                return Err("cannot make the video frame writable".into());
            }
            ffi::sws_scale(
                self.0,
                planes.as_ptr(),
                stride.as_ptr(),
                0,
                src.height as i32,
                (*d).data.as_ptr(),
                (*d).linesize.as_ptr(),
            )
        };
        if ret != src.height as i32 {
            return Err(format!("swscale converted {ret} of {} rows", src.height));
        }
        Ok(())
    }
}

impl Drop for Scaler {
    fn drop(&mut self) {
        // SAFETY: `self.0` came from `sws_getContext` and is freed once.
        unsafe { ffi::sws_freeContext(self.0) }
    }
}

/// AAC encoder state: samples are buffered per channel until a full encoder
/// frame is available.
struct AudioTrack {
    encoder: ff::encoder::Audio,
    stream: usize,
    channels: usize,
    rate: i32,
    frame_size: usize,
    /// Planar pending samples, one `Vec` per channel.
    pending: Vec<Vec<f32>>,
    /// Samples (per channel) sent to the encoder so far: the next frame's pts.
    next_pts: i64,
}

impl AudioTrack {
    fn open(
        settings: &ExportSettings,
        octx: &mut ff::format::context::Output,
        global_header: bool,
    ) -> std::result::Result<Self, String> {
        let codec = ff::encoder::find(ff::codec::Id::AAC).ok_or("no AAC encoder in FFmpeg")?;
        let rate = i32::try_from(settings.audio_sample_rate)
            .map_err(|_| format!("sample rate {} too large", settings.audio_sample_rate))?;
        let channels = settings.audio_channels as i32;
        let mut enc = ff::codec::Context::new_with_codec(codec)
            .encoder()
            .audio()
            .map_err(|e| e.to_string())?;
        enc.set_rate(rate);
        enc.set_format(ff::format::Sample::F32(ff::format::sample::Type::Planar));
        enc.set_time_base(ff::Rational::new(1, rate));
        enc.set_bit_rate(AUDIO_BITRATE);
        // SAFETY: `enc` owns a valid AVCodecContext.
        unsafe { ffi::av_channel_layout_default(&mut (*enc.as_mut_ptr()).ch_layout, channels) };
        if global_header {
            enc.set_flags(ff::codec::Flags::GLOBAL_HEADER);
        }
        let encoder = enc.open().map_err(|e| format!("aac: {e}"))?;
        let frame_size = (encoder.frame_size() as usize).max(1);
        let mut ost = octx.add_stream(codec).map_err(|e| e.to_string())?;
        ost.set_parameters(&encoder);
        ost.set_time_base(ff::Rational::new(1, rate));
        Ok(AudioTrack {
            encoder,
            stream: ost.index(),
            channels: channels as usize,
            rate,
            frame_size,
            pending: vec![Vec::new(); channels as usize],
            next_pts: 0,
        })
    }

    /// Encode one frame of `n` samples from the front of `pending`.
    fn encode_frame(
        &mut self,
        n: usize,
        octx: &mut ff::format::context::Output,
    ) -> Result<(), String> {
        let mut f = ff::frame::Audio::empty();
        // SAFETY: `f` owns a fresh AVFrame; we set its audio fields, allocate
        // its buffers and write `n` samples into each of `channels` planes.
        unsafe {
            let p = f.as_mut_ptr();
            (*p).format = ffi::AVSampleFormat::AV_SAMPLE_FMT_FLTP as i32;
            (*p).nb_samples = n as i32;
            (*p).sample_rate = self.rate;
            ffi::av_channel_layout_default(&mut (*p).ch_layout, self.channels as i32);
            if ffi::av_frame_get_buffer(p, 0) < 0 {
                return Err("cannot allocate an audio frame".into());
            }
            for (ch, samples) in self.pending.iter().enumerate() {
                ptr::copy_nonoverlapping(samples.as_ptr(), (*p).data[ch] as *mut f32, n);
            }
        }
        for samples in &mut self.pending {
            samples.drain(..n);
        }
        f.set_pts(Some(self.next_pts));
        self.next_pts += n as i64;
        self.encoder.send_frame(&f).map_err(|e| e.to_string())?;
        let tb = ff::Rational::new(1, self.rate);
        drain(&mut self.encoder, octx, self.stream, tb)
    }
}

/// Write every packet the encoder has ready.
fn drain(
    enc: &mut ff::encoder::Encoder,
    octx: &mut ff::format::context::Output,
    stream: usize,
    enc_tb: ff::Rational,
) -> Result<(), String> {
    let st_tb = octx
        .stream(stream)
        .ok_or("output stream disappeared")?
        .time_base();
    let mut pkt = ff::Packet::empty();
    while enc.receive_packet(&mut pkt).is_ok() {
        pkt.set_stream(stream);
        pkt.rescale_ts(enc_tb, st_tb);
        pkt.write_interleaved(octx).map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// [`VideoWriter`] over an FFmpeg MP4 muxer.
struct FfmpegWriter {
    path: PathBuf,
    settings: ExportSettings,
    octx: ff::format::context::Output,
    video: OpenedVideo,
    video_stream: usize,
    scaler: Scaler,
    yuv: ff::frame::Video,
    /// Last video pts written, in encoder ticks.
    last_pts: Option<i64>,
    audio: AudioTrack,
}

impl FfmpegWriter {
    fn open(path: &Path, settings: &ExportSettings, require_videotoolbox: bool) -> Result<Self> {
        let err = |message: String| MediaError::Encode {
            path: path.to_path_buf(),
            message,
        };
        FfmpegBackend::new()?;
        let mut octx = ff::format::output_as(path, "mp4").map_err(|e| err(e.to_string()))?;
        let global_header = octx
            .format()
            .flags()
            .contains(ff::format::Flags::GLOBAL_HEADER);

        let video = open_h264(settings, global_header, require_videotoolbox).map_err(err)?;
        let video_stream = {
            let codec = video
                .encoder
                .codec()
                .ok_or_else(|| err("no codec".into()))?;
            let mut ost = octx.add_stream(codec).map_err(|e| err(e.to_string()))?;
            ost.set_parameters(&video.encoder);
            ost.set_time_base(video_time_base(settings));
            ost.set_avg_frame_rate(ff::Rational::new(settings.fps.num, settings.fps.den));
            ost.index()
        };
        let scaler = Scaler::new(settings.width, settings.height, video.pixel).map_err(err)?;
        let yuv = ff::frame::Video::new(video.pixel, settings.width, settings.height);
        let audio = AudioTrack::open(settings, &mut octx, global_header).map_err(err)?;

        let mut opts = ff::Dictionary::new();
        // moov before mdat, so the file streams.
        opts.set("movflags", "+faststart");
        octx.write_header_with(opts)
            .map_err(|e| err(format!("write header: {e}")))?;

        Ok(FfmpegWriter {
            path: path.to_path_buf(),
            settings: settings.clone(),
            octx,
            video,
            video_stream,
            scaler,
            yuv,
            last_pts: None,
            audio,
        })
    }

    fn err(&self, message: impl Into<String>) -> MediaError {
        MediaError::Encode {
            path: self.path.clone(),
            message: message.into(),
        }
    }

    fn drain_video(&mut self) -> Result<()> {
        let tb = video_time_base(&self.settings);
        drain(
            &mut self.video.encoder,
            &mut self.octx,
            self.video_stream,
            tb,
        )
        .map_err(|m| self.err(format!("video: {m}")))
    }
}

impl VideoWriter for FfmpegWriter {
    fn encoder_name(&self) -> &str {
        &self.video.name
    }

    fn write_video(&mut self, frame: &VideoFrame) -> Result<()> {
        let s = &self.settings;
        if frame.format != PixelFormat::Rgba8
            || (frame.width, frame.height) != (s.width, s.height)
            || frame.data.len() != frame.stride() * frame.height as usize
        {
            return Err(self.err(format!(
                "frame is {}x{} {:?} with {} bytes, export needs {}x{} RGBA8",
                frame.width,
                frame.height,
                frame.format,
                frame.data.len(),
                s.width,
                s.height
            )));
        }
        let pts = frame_index(&self.settings, frame.pts);
        if pts < 0 || self.last_pts.is_some_and(|last| pts <= last) {
            return Err(self.err(format!(
                "frame pts {:?} (frame {pts}) is negative or not after the previous frame",
                frame.pts
            )));
        }
        self.scaler
            .convert(frame, &mut self.yuv)
            .map_err(|m| self.err(m))?;
        self.yuv.set_pts(Some(pts));
        // SAFETY: `self.yuv` owns a valid AVFrame; one frame lasts one tick.
        unsafe { (*self.yuv.as_mut_ptr()).duration = 1 };
        self.video
            .encoder
            .send_frame(&self.yuv)
            .map_err(|e| self.err(format!("video: {e}")))?;
        self.last_pts = Some(pts);
        self.drain_video()
    }

    fn write_audio(&mut self, interleaved: &[f32]) -> Result<()> {
        let channels = self.audio.channels;
        if interleaved.len() % channels != 0 {
            return Err(self.err(format!(
                "{} audio samples is not a whole number of {channels}-channel frames",
                interleaved.len()
            )));
        }
        for frame in interleaved.chunks_exact(channels) {
            for (plane, &sample) in self.audio.pending.iter_mut().zip(frame) {
                plane.push(sample);
            }
        }
        while self.audio.pending[0].len() >= self.audio.frame_size {
            let n = self.audio.frame_size;
            self.audio
                .encode_frame(n, &mut self.octx)
                .map_err(|m| self.err(format!("audio: {m}")))?;
        }
        Ok(())
    }

    fn finish(mut self: Box<Self>) -> Result<()> {
        self.video
            .encoder
            .send_eof()
            .map_err(|e| self.err(format!("video: {e}")))?;
        self.drain_video()?;

        let rest = self.audio.pending[0].len();
        if rest > 0 {
            self.audio
                .encode_frame(rest, &mut self.octx)
                .map_err(|m| self.err(format!("audio: {m}")))?;
        }
        self.audio
            .encoder
            .send_eof()
            .map_err(|e| self.err(format!("audio: {e}")))?;
        let tb = ff::Rational::new(1, self.audio.rate);
        drain(
            &mut self.audio.encoder,
            &mut self.octx,
            self.audio.stream,
            tb,
        )
        .map_err(|m| self.err(format!("audio: {m}")))?;

        self.octx
            .write_trailer()
            .map_err(|e| self.err(format!("write trailer: {e}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Rational;

    fn settings() -> ExportSettings {
        ExportSettings {
            width: 64,
            height: 48,
            video_bitrate: 500_000,
            ..ExportSettings::default()
        }
    }

    #[test]
    fn candidate_order() {
        let names: Vec<_> = h264_candidates().into_iter().map(|(n, _)| n).collect();
        if cfg!(target_os = "macos") {
            assert_eq!(names, [VIDEOTOOLBOX, "libopenh264", "libx264"]);
        } else {
            assert_eq!(names, ["libopenh264", "libx264"]);
        }
    }

    #[test]
    fn requiring_videotoolbox_never_falls_back() {
        FfmpegBackend::new().unwrap();
        match open_h264(&settings(), true, true) {
            // A real Mac with a hardware encoder.
            Ok(v) => assert_eq!(v.name, VIDEOTOOLBOX),
            Err(e) => assert!(e.contains(REQUIRE_VIDEOTOOLBOX_ENV), "{e}"),
        }
    }

    #[test]
    fn frame_index_rounds_to_nearest_frame() {
        let s = ExportSettings {
            fps: Rational::new(30000, 1001),
            ..settings()
        };
        let idx = |us| frame_index(&s, kv_core::Micros(us));
        assert_eq!(idx(0), 0);
        assert_eq!(idx(33_366), 1);
        assert_eq!(idx(33_400), 1);
        assert_eq!(idx(1_001_000), 30);
        assert_eq!(idx(1_000_990), 30);
        assert_eq!(idx(-33_366), -1);
    }
}
