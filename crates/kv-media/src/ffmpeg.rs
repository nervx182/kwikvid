//! [`MediaBackend`] implemented with the system FFmpeg libraries through
//! `ffmpeg-next`.
//!
//! Requires FFmpeg 6.1 or newer (codec parameters carry their side data
//! since 6.1). Shipping builds must link an LGPL FFmpeg (ADR 0005).

// `ffmpeg-next` does not expose every field we need (pixel format, channel
// count, display matrix side data), so this module reads them from the raw
// FFmpeg structs. Each `unsafe` block only reads from pointers owned by a
// live `ffmpeg-next` wrapper.
#![allow(unsafe_code)]

use std::path::Path;
use std::sync::OnceLock;

use ff::ffi;
use ffmpeg_next as ff;
use kv_core::Micros;

use crate::{
    parse_creation_time, rotation_from_display_matrix, AudioInfo, MediaBackend, MediaError,
    MediaInfo, Rational, Result, VideoInfo,
};

/// FFmpeg-based [`MediaBackend`].
#[derive(Debug, Clone, Copy)]
pub struct FfmpegBackend {
    _private: (),
}

impl FfmpegBackend {
    /// Initialise FFmpeg (once per process) and return the backend.
    pub fn new() -> Result<Self> {
        static INIT: OnceLock<Result<(), String>> = OnceLock::new();
        INIT.get_or_init(|| {
            ff::init().map_err(|e| e.to_string())?;
            ff::util::log::set_level(ff::util::log::Level::Error);
            Ok(())
        })
        .clone()
        .map_err(MediaError::Init)?;
        Ok(FfmpegBackend { _private: () })
    }
}

impl MediaBackend for FfmpegBackend {
    fn probe(&self, path: &Path) -> Result<MediaInfo> {
        let open_err = |message: String| MediaError::Open {
            path: path.to_path_buf(),
            message,
        };
        let ictx = ff::format::input(path).map_err(|e| open_err(e.to_string()))?;

        let video = ictx
            .streams()
            .best(ff::media::Type::Video)
            .map(|s| video_info(&s))
            .transpose()
            .map_err(open_err)?;
        let audio = ictx
            .streams()
            .best(ff::media::Type::Audio)
            .map(|s| audio_info(&s));
        if video.is_none() && audio.is_none() {
            return Err(MediaError::NoStreams(path.to_path_buf()));
        }

        let creation_time = ictx
            .metadata()
            .get("creation_time")
            .and_then(parse_creation_time)
            .or_else(|| {
                ictx.streams().find_map(|s| {
                    s.metadata()
                        .get("creation_time")
                        .and_then(parse_creation_time)
                })
            });

        Ok(MediaInfo {
            duration: duration(&ictx),
            container: ictx.format().name().to_string(),
            video,
            audio,
            creation_time,
        })
    }
}

/// Container duration (already in `AV_TIME_BASE` = microseconds), falling
/// back to the longest stream.
fn duration(ictx: &ff::format::context::Input) -> Micros {
    let d = ictx.duration();
    if d != ffi::AV_NOPTS_VALUE && d > 0 {
        return Micros(d);
    }
    let longest = ictx
        .streams()
        .filter(|s| s.duration() != ffi::AV_NOPTS_VALUE && s.duration() > 0)
        .map(|s| {
            let tb = s.time_base();
            // duration * tb in microseconds, without overflow for sane inputs.
            (i128::from(s.duration()) * i128::from(tb.numerator()) * 1_000_000
                / i128::from(tb.denominator().max(1))) as i64
        })
        .max()
        .unwrap_or(0);
    Micros(longest)
}

fn video_info(stream: &ff::format::stream::Stream) -> Result<VideoInfo, String> {
    let params = stream.parameters();
    let ctx = ff::codec::Context::from_parameters(params.clone()).map_err(|e| e.to_string())?;
    // SAFETY: `ctx` owns a valid AVCodecContext for the duration of the read.
    let (width, height, pix_fmt, raw_bits) = unsafe {
        let c = &*ctx.as_ptr();
        (c.width, c.height, c.pix_fmt, c.bits_per_raw_sample)
    };
    let bit_depth = ff::format::Pixel::from(pix_fmt)
        .descriptor()
        // SAFETY: the descriptor points into FFmpeg's static pixel format table.
        .map(|d| unsafe { (*d.as_ptr()).comp[0].depth })
        .filter(|&d| d > 0)
        .unwrap_or(raw_bits);

    let rate = Some(stream.avg_frame_rate())
        .filter(|r| r.numerator() > 0 && r.denominator() > 0)
        .unwrap_or_else(|| stream.rate());

    Ok(VideoInfo {
        codec: params.id().name().to_string(),
        width: u32::try_from(width).unwrap_or(0),
        height: u32::try_from(height).unwrap_or(0),
        fps: Rational::new(rate.numerator(), rate.denominator()),
        bit_depth: u8::try_from(bit_depth).unwrap_or(0),
        rotation: display_matrix(&params)
            .map(|m| rotation_from_display_matrix(&m))
            .unwrap_or(0),
    })
}

fn audio_info(stream: &ff::format::stream::Stream) -> AudioInfo {
    let params = stream.parameters();
    // SAFETY: `params` owns a valid AVCodecParameters for the duration of the read.
    let (sample_rate, channels) = unsafe {
        let p = &*params.as_ptr();
        (p.sample_rate, p.ch_layout.nb_channels)
    };
    AudioInfo {
        codec: params.id().name().to_string(),
        sample_rate: u32::try_from(sample_rate).unwrap_or(0),
        channels: u16::try_from(channels).unwrap_or(0),
    }
}

/// The stream's display matrix side data, if present.
fn display_matrix(params: &ff::codec::Parameters) -> Option<[i32; 9]> {
    // SAFETY: `params` owns a valid AVCodecParameters; the returned side data
    // (if any) lives in it and is only read while `params` is alive.
    unsafe {
        let p = &*params.as_ptr();
        let sd = ffi::av_packet_side_data_get(
            p.coded_side_data,
            p.nb_coded_side_data,
            ffi::AVPacketSideDataType::AV_PKT_DATA_DISPLAYMATRIX,
        );
        if sd.is_null() || (*sd).size < 36 || (*sd).data.is_null() {
            return None;
        }
        let bytes = std::slice::from_raw_parts((*sd).data, 36);
        let mut m = [0i32; 9];
        for (v, chunk) in m.iter_mut().zip(bytes.chunks_exact(4)) {
            *v = i32::from_ne_bytes(chunk.try_into().expect("4-byte chunk"));
        }
        Some(m)
    }
}
