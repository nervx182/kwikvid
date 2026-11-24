//! kwikvid platform media layer.
//!
//! Everything that touches media files sits behind [`MediaBackend`] so the
//! engine crates stay free of I/O and platform code (see
//! `docs/architecture.md`). The trait probes files and decodes video;
//! thumbnails, proxies and export come later.
//!
//! The default backend is [`FfmpegBackend`] (cargo feature `ffmpeg`, on by
//! default), which needs the system FFmpeg libraries; see this crate's
//! `README.md`.

use std::path::{Path, PathBuf};

use kv_core::Micros;
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[cfg(feature = "ffmpeg")]
mod ffmpeg;
#[cfg(feature = "ffmpeg")]
mod ffmpeg_decode;
mod frame;
#[cfg(feature = "ffmpeg")]
pub use ffmpeg::FfmpegBackend;
#[cfg(feature = "ffmpeg")]
mod ffmpeg_encode;
#[cfg(feature = "ffmpeg")]
pub use ffmpeg_decode::FfmpegVideoReader;
#[cfg(feature = "ffmpeg")]
pub use ffmpeg_encode::REQUIRE_VIDEOTOOLBOX_ENV;
pub use frame::{ExportSettings, PixelFormat, VideoCodec, VideoFrame, MAX_EXPORT_DIMENSION};

/// Result alias for this crate.
pub type Result<T, E = MediaError> = std::result::Result<T, E>;

/// Why a media operation failed.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum MediaError {
    /// The backend library could not be initialised.
    #[error("media backend failed to initialise: {0}")]
    Init(String),

    /// The file could not be opened or its container was not recognised.
    #[error("cannot open {path:?}: {message}")]
    Open { path: PathBuf, message: String },

    /// The file opened but holds no audio or video stream.
    #[error("{0:?} has no audio or video stream")]
    NoStreams(PathBuf),

    /// A [`VideoFrame`] was built from data that does not match its size and
    /// pixel format.
    #[error("invalid video frame: {0}")]
    InvalidFrame(String),

    /// [`ExportSettings::validate`] found settings that cannot be encoded.
    #[error("invalid export settings: {0}")]
    InvalidExportSettings(String),

    /// Writing an export failed: the output could not be created, no
    /// encoder could be opened, or a frame or sample buffer was rejected.
    #[error("cannot encode {path:?}: {message}")]
    Encode { path: PathBuf, message: String },

    /// The requested operation needs an encoder or feature this system does
    /// not have, e.g. HEVC export without VideoToolbox.
    #[error("unsupported: {0}")]
    Unsupported(String),

    /// The file opened but holds no video stream.
    #[error("{0:?} has no video stream")]
    NoVideoStream(PathBuf),

    /// Decoding failed (corrupt or truncated data, unsupported codec).
    #[error("cannot decode {path:?}: {message}")]
    Decode { path: PathBuf, message: String },
}

/// A platform media backend. Implementations must be usable from background
/// worker threads.
pub trait MediaBackend: Send + Sync {
    /// Read container and stream metadata without decoding frames.
    fn probe(&self, path: &Path) -> Result<MediaInfo>;

    /// Open the primary video stream of `path` for decoding to RGBA frames.
    fn open_video(&self, path: &Path, opts: &DecodeOptions) -> Result<Box<dyn VideoReader>>;
}

/// Metadata of one media file, as returned by [`MediaBackend::probe`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MediaInfo {
    /// Total duration of the file.
    pub duration: Micros,
    /// Container (demuxer) name as reported by the backend, e.g.
    /// `"mov,mp4,m4a,3gp,3g2,mj2"` or `"matroska,webm"` for FFmpeg.
    pub container: String,
    /// The primary video stream, if any.
    pub video: Option<VideoInfo>,
    /// The primary audio stream, if any.
    pub audio: Option<AudioInfo>,
    /// When the recording was made, in microseconds since the Unix epoch
    /// (UTC), from the container's `creation_time` tag.
    pub creation_time: Option<Micros>,
}

/// Metadata of a video stream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VideoInfo {
    /// Codec name, e.g. `"h264"` or `"hevc"`.
    pub codec: String,
    /// Coded width in pixels (before rotation).
    pub width: u32,
    /// Coded height in pixels (before rotation).
    pub height: u32,
    /// Average frame rate, e.g. 30000/1001.
    pub fps: Rational,
    /// Bits per colour component of the decoded pixel format (8, 10, ...).
    pub bit_depth: u8,
    /// Clockwise rotation in degrees (0, 90, 180 or 270) to apply to decoded
    /// frames for upright display, from the stream's display matrix.
    pub rotation: u16,
}

/// Metadata of an audio stream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AudioInfo {
    /// Codec name, e.g. `"aac"`.
    pub codec: String,
    /// Samples per second.
    pub sample_rate: u32,
    /// Number of channels.
    pub channels: u16,
}

/// An exact rational number, used for frame rates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Rational {
    pub num: i32,
    pub den: i32,
}

impl Rational {
    pub const fn new(num: i32, den: i32) -> Self {
        Rational { num, den }
    }

    /// Value as a float; 0 when the denominator is 0.
    pub fn as_f64(self) -> f64 {
        if self.den == 0 {
            0.0
        } else {
            f64::from(self.num) / f64::from(self.den)
        }
    }
}

/// Clockwise display rotation (0, 90, 180 or 270) from a 3x3 display matrix
/// as stored in MP4/MOV `tkhd` boxes and FFmpeg's `AV_PKT_DATA_DISPLAYMATRIX`
/// side data (row-major, 16.16 fixed point except the last column, 2.30).
///
/// `av_display_rotation_get` returns the angle the matrix rotates the frame
/// counterclockwise (ffprobe prints it as `rotation`; an upright phone video
/// shows -90). This returns its negation snapped to a quarter turn, which is
/// the rotation FFmpeg's autorotate applies (ffprobe -90 gives 90 here).
pub fn rotation_from_display_matrix(m: &[i32; 9]) -> u16 {
    let fp = |v: i32| f64::from(v) / f64::from(1 << 16);
    let scale0 = fp(m[0]).hypot(fp(m[3]));
    let scale1 = fp(m[1]).hypot(fp(m[4]));
    if scale0 == 0.0 || scale1 == 0.0 {
        return 0;
    }
    // `av_display_rotation_get` is the negation of this angle.
    let cw = (fp(m[1]) / scale1).atan2(fp(m[0]) / scale0).to_degrees();
    let cw = (cw / 90.0).round() as i64 * 90;
    cw.rem_euclid(360) as u16
}

/// Parse an ISO 8601 / RFC 3339 timestamp as written in container
/// `creation_time` tags (`2024-05-06T07:08:09.000000Z`, optionally with a
/// space instead of `T`, no fraction, or a `+HH:MM` offset) into microseconds
/// since the Unix epoch. A timestamp without a zone is taken as UTC.
pub fn parse_creation_time(s: &str) -> Option<Micros> {
    let s = s.trim();
    let b = s.as_bytes();
    if b.len() < 19 || b[4] != b'-' || b[7] != b'-' || !matches!(b[10], b'T' | b't' | b' ') {
        return None;
    }
    if b[13] != b':' || b[16] != b':' {
        return None;
    }
    let num = |r: std::ops::Range<usize>| -> Option<i64> {
        let part = s.get(r)?;
        if part.bytes().all(|c| c.is_ascii_digit()) {
            part.parse().ok()
        } else {
            None
        }
    };
    let (year, month, day) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (hour, min, sec) = (num(11..13)?, num(14..16)?, num(17..19)?);
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) || hour > 23 || min > 59 || sec > 60 {
        return None;
    }

    let mut rest = &s[19..];
    let mut micros = 0i64;
    if let Some(frac) = rest.strip_prefix('.') {
        let digits = frac.bytes().take_while(u8::is_ascii_digit).count();
        if digits == 0 {
            return None;
        }
        for (i, c) in frac.bytes().take(digits).enumerate() {
            if i < 6 {
                micros = micros * 10 + i64::from(c - b'0');
            }
        }
        for _ in digits..6 {
            micros *= 10;
        }
        rest = &frac[digits..];
    }

    let offset_secs = match rest {
        "" | "Z" | "z" => 0,
        _ => {
            let sign = match rest.as_bytes()[0] {
                b'+' => 1,
                b'-' => -1,
                _ => return None,
            };
            let hm = rest[1..].replace(':', "");
            if hm.len() != 4 || !hm.bytes().all(|c| c.is_ascii_digit()) {
                return None;
            }
            let h: i64 = hm[0..2].parse().ok()?;
            let m: i64 = hm[2..4].parse().ok()?;
            sign * (h * 3600 + m * 60)
        }
    };

    let days = days_from_civil(year, month, day);
    let secs = days * 86_400 + hour * 3600 + min * 60 + sec - offset_secs;
    Some(Micros(secs * 1_000_000 + micros))
}

/// Days since 1970-01-01 for a proleptic Gregorian date (Howard Hinnant's
/// `days_from_civil`).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Creates [`VideoWriter`]s: the export side of a platform backend.
pub trait Encoder: Send + Sync {
    /// Start writing an MP4 at `path`. Calls [`ExportSettings::validate`]
    /// first. Fails with [`MediaError::Unsupported`], before touching the
    /// file, when [`supports`](Self::supports) is false for the codec, and
    /// with [`MediaError::Encode`] when the file cannot be created or no
    /// encoder is usable.
    fn create(&self, path: &Path, settings: &ExportSettings) -> Result<Box<dyn VideoWriter>>;

    /// Whether this system can encode `codec`, so a UI can hide the codecs
    /// [`create`](Self::create) would reject as unsupported.
    fn supports(&self, codec: VideoCodec) -> bool;
}

/// One export in progress. Dropping it without [`VideoWriter::finish`]
/// leaves an incomplete file but never panics.
pub trait VideoWriter: Send {
    /// Name of the video encoder in use, e.g. `"h264_videotoolbox"`.
    fn encoder_name(&self) -> &str;
    /// Encode the next frame. Frames arrive in order, at exactly the export
    /// frame rate; the frame's `pts` (rounded to the nearest frame) is used
    /// as its timestamp and must increase.
    fn write_video(&mut self, frame: &VideoFrame) -> Result<()>;
    /// Append interleaved PCM (`audio_channels` samples per frame, nominal
    /// range -1..=1) in chunks of any size. Audio starts at time 0.
    fn write_audio(&mut self, interleaved: &[f32]) -> Result<()>;
    /// Flush the encoders and finalise the file.
    fn finish(self: Box<Self>) -> Result<()>;
}

/// How [`MediaBackend::open_video`] converts decoded frames.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct DecodeOptions {
    /// Output size in pixels. `None` keeps the display size (the coded size,
    /// with width and height swapped for a 90/270 rotation when
    /// `apply_rotation` is set). `Some((w, h))` scales to exactly `w x h`
    /// with no aspect handling: cropping and fitting are the compositor's job.
    pub size: Option<(u32, u32)>,
    /// Apply the display rotation from [`VideoInfo::rotation`] so frames come
    /// out upright. The rotation happens before scaling to `size`.
    pub apply_rotation: bool,
}

impl Default for DecodeOptions {
    /// Display size, upright.
    fn default() -> Self {
        DecodeOptions {
            size: None,
            apply_rotation: true,
        }
    }
}

/// Decodes one video stream into [`VideoFrame`]s (RGBA8), in presentation
/// order. Frame times are relative to the stream start, so the first frame
/// of a normal file is at or near 0.
pub trait VideoReader: Send {
    /// Metadata of the stream being decoded (as [`MediaBackend::probe`]
    /// reports it: coded size, rotation, ...).
    fn info(&self) -> &VideoInfo;

    /// Reposition so that [`next_frame`](Self::next_frame) returns the frame
    /// displayed at `t` (the one with the largest pts `<= t`; the first frame
    /// when `t` is before it), followed by the later frames.
    fn seek(&mut self, t: Micros) -> Result<()>;

    /// The next frame in presentation order, `None` after the last one.
    fn next_frame(&mut self) -> Result<Option<VideoFrame>>;

    /// The frame displayed at `t`: the one with the largest pts `<= t` (so
    /// the last frame for any `t` past the end). `None` when `t` is before
    /// the first frame. Decodes forward from the current position when `t`
    /// is ahead and close; otherwise seeks to the keyframe before `t` and
    /// decodes forward. Calls with increasing `t` therefore do not re-seek.
    fn frame_at(&mut self, t: Micros) -> Result<Option<VideoFrame>>;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a display matrix exactly like `av_display_rotation_set(m, angle)`.
    fn matrix_set(angle: f64) -> [i32; 9] {
        let r = (-angle).to_radians();
        let (c, s) = (r.cos(), r.sin());
        let fx = |v: f64| (v * f64::from(1 << 16)).round() as i32;
        [fx(c), fx(-s), 0, fx(s), fx(c), 0, 0, 0, 1 << 30]
    }

    #[test]
    fn rotation_identity_is_zero() {
        assert_eq!(rotation_from_display_matrix(&matrix_set(0.0)), 0);
    }

    #[test]
    fn rotation_is_clockwise_quarter_turns() {
        // An upright phone video: ffprobe shows rotation -90, written with
        // av_display_rotation_set(90); display needs 90 degrees clockwise.
        assert_eq!(rotation_from_display_matrix(&matrix_set(90.0)), 90);
        assert_eq!(rotation_from_display_matrix(&matrix_set(-90.0)), 270);
        assert_eq!(rotation_from_display_matrix(&matrix_set(180.0)), 180);
        assert_eq!(rotation_from_display_matrix(&matrix_set(-180.0)), 180);
        assert_eq!(rotation_from_display_matrix(&matrix_set(88.0)), 90);
    }

    #[test]
    fn rotation_of_degenerate_matrix_is_zero() {
        assert_eq!(rotation_from_display_matrix(&[0; 9]), 0);
    }

    #[test]
    fn creation_time_formats() {
        let base = Micros(1_714_979_289_000_000); // 2024-05-06T07:08:09Z
        assert_eq!(
            parse_creation_time("2024-05-06T07:08:09.000000Z"),
            Some(base)
        );
        assert_eq!(parse_creation_time("2024-05-06T07:08:09Z"), Some(base));
        assert_eq!(parse_creation_time("2024-05-06 07:08:09"), Some(base));
        assert_eq!(
            parse_creation_time("2024-05-06T07:08:09.5Z"),
            Some(Micros(base.0 + 500_000))
        );
        assert_eq!(
            parse_creation_time("2024-05-06T07:08:09.123456789Z"),
            Some(Micros(base.0 + 123_456))
        );
        assert_eq!(parse_creation_time("2024-05-06T09:08:09+02:00"), Some(base));
        assert_eq!(parse_creation_time("2024-05-06T05:08:09-0200"), Some(base));
        assert_eq!(parse_creation_time("1970-01-01T00:00:00Z"), Some(Micros(0)));
        assert_eq!(
            parse_creation_time("1969-12-31T23:59:59Z"),
            Some(Micros(-1_000_000))
        );
    }

    #[test]
    fn creation_time_rejects_garbage() {
        for s in [
            "",
            "now",
            "2024-13-06T07:08:09Z",
            "2024-05-06X07:08:09Z",
            "2024-05-06T07:08:09.Z",
            "2024-05-06T07:08:09+2",
            "2024-05-06T07:08:09 UTC",
            "２０２４-05-06T07:08:09Z",
        ] {
            assert_eq!(parse_creation_time(s), None, "{s:?}");
        }
    }

    #[test]
    fn rational_as_f64() {
        assert!((Rational::new(30000, 1001).as_f64() - 29.97).abs() < 0.001);
        assert_eq!(Rational::new(1, 0).as_f64(), 0.0);
    }
}
