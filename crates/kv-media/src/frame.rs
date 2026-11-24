//! Plain-data frame and export types shared by decode, encode and the
//! compositor. No FFmpeg or platform code: always compiled, also with
//! `--no-default-features`.

use kv_core::Micros;
use serde::{Deserialize, Serialize};

use crate::{MediaError, Rational, Result};

/// Pixel layout of a [`VideoFrame`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PixelFormat {
    /// 8-bit red, green, blue, alpha; tightly packed (`stride == width * 4`),
    /// sRGB, straight (not premultiplied) alpha. Video frames always have
    /// alpha 255.
    Rgba8,
}

impl PixelFormat {
    /// Bytes per pixel.
    pub const fn bytes_per_pixel(self) -> usize {
        match self {
            PixelFormat::Rgba8 => 4,
        }
    }
}

/// One decoded (or to-be-encoded) video frame in CPU memory.
#[derive(Debug, Clone, PartialEq)]
pub struct VideoFrame {
    /// Presentation time of the frame.
    pub pts: Micros,
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Layout of `data`.
    pub format: PixelFormat,
    /// Pixel bytes, row-major, top row first, no row padding.
    pub data: Vec<u8>,
}

/// Exact byte length of a tightly packed frame, or `None` on overflow.
fn frame_len(width: u32, height: u32, format: PixelFormat) -> Option<usize> {
    usize::try_from(width)
        .ok()?
        .checked_mul(usize::try_from(height).ok()?)?
        .checked_mul(format.bytes_per_pixel())
}

impl VideoFrame {
    /// Wrap tightly packed RGBA8 bytes. Fails with
    /// [`MediaError::InvalidFrame`] unless `data.len() == width * height * 4`.
    pub fn new_rgba(pts: Micros, width: u32, height: u32, data: Vec<u8>) -> Result<Self> {
        let expected = frame_len(width, height, PixelFormat::Rgba8);
        if expected != Some(data.len()) {
            return Err(MediaError::InvalidFrame(format!(
                "{width}x{height} RGBA8 needs {} bytes, got {}",
                expected.map_or_else(|| "too many".to_owned(), |n| n.to_string()),
                data.len()
            )));
        }
        Ok(VideoFrame {
            pts,
            width,
            height,
            format: PixelFormat::Rgba8,
            data,
        })
    }

    /// An RGBA8 frame filled with one colour; handy in tests.
    ///
    /// # Panics
    /// If `width * height * 4` overflows `usize`.
    pub fn solid(pts: Micros, width: u32, height: u32, rgba: [u8; 4]) -> Self {
        let len = frame_len(width, height, PixelFormat::Rgba8).expect("frame size overflows usize");
        let data = rgba.repeat(len / 4);
        VideoFrame {
            pts,
            width,
            height,
            format: PixelFormat::Rgba8,
            data,
        }
    }

    /// Bytes per row (`width * bytes_per_pixel`; frames have no padding).
    pub fn stride(&self) -> usize {
        self.width as usize * self.format.bytes_per_pixel()
    }
}

/// Video codec of an export.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum VideoCodec {
    /// H.264 / AVC.
    H264,
    /// H.265 / HEVC (8-bit Main profile). Only encodable where an
    /// LGPL-compatible encoder exists (VideoToolbox on macOS); check
    /// [`Encoder::supports`](crate::Encoder::supports).
    #[serde(rename = "hevc", alias = "Hevc")]
    Hevc,
}

/// Largest export width or height in pixels: covers 4K UHD in both
/// orientations (3840x2160, 2160x3840) and DCI 4K (4096x2160).
pub const MAX_EXPORT_DIMENSION: u32 = 4096;

/// Pixel count of 1920x1080, the reference size for [`ExportSettings::default_bitrate`].
const PIXELS_1080P: u64 = 1920 * 1080;
/// Pixel count of 3840x2160.
const PIXELS_2160P: u64 = 3840 * 2160;
/// H.264 bitrate at 1080p (guess: close to what Quik and phone cameras use
/// for a 1080p30 share export).
const H264_1080P_BITRATE: u64 = 12_000_000;
/// H.264 bitrate at 4K UHD (guess).
const H264_2160P_BITRATE: u64 = 45_000_000;
/// Lowest default bitrate, so tiny sizes still get a usable stream (guess).
const MIN_DEFAULT_BITRATE: u64 = 1_000_000;
/// HEVC bitrate as a percentage of H.264 for similar quality (guess: the
/// usual 40% saving claimed for HEVC over H.264).
const HEVC_PERCENT_OF_H264: u64 = 60;

/// Options for exporting an edit to a file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExportSettings {
    /// Output width in pixels (even, non-zero).
    pub width: u32,
    /// Output height in pixels (even, non-zero).
    pub height: u32,
    /// Output frame rate.
    pub fps: Rational,
    /// Video codec.
    pub video_codec: VideoCodec,
    /// Target video bitrate in bits per second.
    pub video_bitrate: u32,
    /// Audio samples per second.
    pub audio_sample_rate: u32,
    /// Audio channels (1 or 2).
    pub audio_channels: u16,
}

impl Default for ExportSettings {
    /// 1920x1080, 30 fps, H.264 at 12 Mbit/s (guess), 48 kHz stereo.
    fn default() -> Self {
        ExportSettings {
            width: 1920,
            height: 1080,
            fps: Rational::new(30, 1),
            video_codec: VideoCodec::H264,
            video_bitrate: 12_000_000,
            audio_sample_rate: 48_000,
            audio_channels: 2,
        }
    }
}

impl ExportSettings {
    /// Default video bitrate in bits per second for a `width x height`
    /// export with `codec`, at about 30 fps. All numbers are guesses, to be
    /// tuned against real exports:
    ///
    /// - H.264: 12 Mbit/s at 1080p (1920x1080 pixels or fewer scale down
    ///   with the pixel count, at least 1 Mbit/s) and 45 Mbit/s at 4K
    ///   (3840x2160), linear in the pixel count between and beyond.
    /// - HEVC: 60% of the H.264 value.
    ///
    /// Only the pixel count matters, so portrait and landscape sizes get the
    /// same bitrate.
    pub fn default_bitrate(width: u32, height: u32, codec: VideoCodec) -> u32 {
        let pixels = u64::from(width) * u64::from(height);
        let h264 = if pixels <= PIXELS_1080P {
            (H264_1080P_BITRATE * pixels / PIXELS_1080P).max(MIN_DEFAULT_BITRATE)
        } else {
            H264_1080P_BITRATE
                + (H264_2160P_BITRATE - H264_1080P_BITRATE) * (pixels - PIXELS_1080P)
                    / (PIXELS_2160P - PIXELS_1080P)
        };
        let bitrate = match codec {
            VideoCodec::H264 => h264,
            VideoCodec::Hevc => h264 * HEVC_PERCENT_OF_H264 / 100,
        };
        u32::try_from(bitrate).unwrap_or(u32::MAX)
    }

    /// Check the settings can be encoded. Rejects a zero or odd width or
    /// height (4:2:0 needs even sizes), a width or height above
    /// [`MAX_EXPORT_DIMENSION`], a zero, negative or undefined frame rate,
    /// and channel counts outside 1..=2. Whether the codec is available on
    /// this system is [`Encoder::supports`](crate::Encoder::supports)'s job.
    pub fn validate(&self) -> Result<()> {
        let invalid = |msg: String| Err(MediaError::InvalidExportSettings(msg));
        for (name, v) in [("width", self.width), ("height", self.height)] {
            if v == 0 || v % 2 != 0 {
                return invalid(format!("{name} must be even and non-zero, got {v}"));
            }
            if v > MAX_EXPORT_DIMENSION {
                return invalid(format!(
                    "{name} must be at most {MAX_EXPORT_DIMENSION}, got {v}"
                ));
            }
        }
        if self.fps.num <= 0 || self.fps.den <= 0 {
            return invalid(format!(
                "fps must be positive, got {}/{}",
                self.fps.num, self.fps.den
            ));
        }
        if !(1..=2).contains(&self.audio_channels) {
            return invalid(format!(
                "audio_channels must be 1 or 2, got {}",
                self.audio_channels
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_rgba_accepts_exact_length() {
        let f = VideoFrame::new_rgba(Micros(40), 3, 2, vec![7; 24]).unwrap();
        assert_eq!(f.pts, Micros(40));
        assert_eq!((f.width, f.height), (3, 2));
        assert_eq!(f.format, PixelFormat::Rgba8);
        assert_eq!(f.stride(), 12);
        assert_eq!(f.data.len(), 24);
    }

    #[test]
    fn new_rgba_rejects_wrong_length() {
        for len in [0, 23, 25, 18] {
            let err = VideoFrame::new_rgba(Micros(0), 3, 2, vec![0; len]).unwrap_err();
            assert!(matches!(err, MediaError::InvalidFrame(_)), "{len}: {err}");
        }
    }

    #[test]
    fn new_rgba_empty_frame_is_valid() {
        assert!(VideoFrame::new_rgba(Micros(0), 0, 5, Vec::new()).is_ok());
    }

    #[test]
    fn new_rgba_overflowing_size_is_rejected() {
        let err = VideoFrame::new_rgba(Micros(0), u32::MAX, u32::MAX, vec![0; 4]);
        if usize::BITS <= 64 {
            assert!(matches!(err, Err(MediaError::InvalidFrame(_))));
        }
    }

    #[test]
    fn solid_fills_every_pixel() {
        let f = VideoFrame::solid(Micros(1), 4, 3, [10, 20, 30, 255]);
        assert_eq!(f.data.len(), 48);
        assert!(f.data.chunks_exact(4).all(|p| p == [10, 20, 30, 255]));
        assert_eq!(
            VideoFrame::new_rgba(Micros(1), 4, 3, f.data.clone()).unwrap(),
            f
        );
    }

    #[test]
    fn default_export_settings() {
        let s = ExportSettings::default();
        assert_eq!((s.width, s.height), (1920, 1080));
        assert_eq!(s.fps, Rational::new(30, 1));
        assert_eq!(s.video_codec, VideoCodec::H264);
        assert_eq!(s.video_bitrate, 12_000_000);
        assert_eq!(s.audio_sample_rate, 48_000);
        assert_eq!(s.audio_channels, 2);
        assert_eq!(s.validate(), Ok(()));
    }

    #[test]
    fn validate_accepts_other_sane_settings() {
        let s = ExportSettings {
            width: 1080,
            height: 1920,
            fps: Rational::new(30000, 1001),
            audio_channels: 1,
            ..ExportSettings::default()
        };
        assert_eq!(s.validate(), Ok(()));
    }

    #[test]
    fn validate_accepts_4k_in_both_orientations_and_hevc() {
        for (width, height) in [(3840, 2160), (2160, 3840), (4096, 2160), (4096, 4096)] {
            for video_codec in [VideoCodec::H264, VideoCodec::Hevc] {
                let s = ExportSettings {
                    width,
                    height,
                    video_codec,
                    video_bitrate: ExportSettings::default_bitrate(width, height, video_codec),
                    ..ExportSettings::default()
                };
                assert_eq!(s.validate(), Ok(()), "{s:?}");
            }
        }
    }

    #[test]
    fn validate_rejects_sizes_above_4096() {
        let d = ExportSettings::default();
        for (width, height) in [(4098, 2160), (2160, 4098), (4098, 4098), (7680, 4320)] {
            let s = ExportSettings {
                width,
                height,
                ..d.clone()
            };
            assert!(
                matches!(s.validate(), Err(MediaError::InvalidExportSettings(_))),
                "{s:?}"
            );
        }
    }

    #[test]
    fn default_bitrates() {
        let br = ExportSettings::default_bitrate;
        assert_eq!(br(1920, 1080, VideoCodec::H264), 12_000_000);
        assert_eq!(br(1080, 1920, VideoCodec::H264), 12_000_000);
        assert_eq!(br(3840, 2160, VideoCodec::H264), 45_000_000);
        assert_eq!(br(2160, 3840, VideoCodec::H264), 45_000_000);
        assert_eq!(br(1920, 1080, VideoCodec::Hevc), 7_200_000);
        assert_eq!(br(3840, 2160, VideoCodec::Hevc), 27_000_000);
        // Smaller sizes scale down, with a floor.
        assert_eq!(br(960, 540, VideoCodec::H264), 3_000_000);
        assert_eq!(br(64, 48, VideoCodec::H264), 1_000_000);
        // Monotonic in the pixel count, up to the largest valid size.
        let h264 = br(4096, 4096, VideoCodec::H264);
        assert!(h264 > 45_000_000 && h264 < 100_000_000, "{h264}");
        assert!(br(1080, 1080, VideoCodec::H264) < 12_000_000);
        assert!(br(2560, 1440, VideoCodec::H264) > 12_000_000);
        // Default settings use the 1080p H.264 default.
        assert_eq!(
            ExportSettings::default().video_bitrate,
            br(1920, 1080, VideoCodec::H264)
        );
    }

    #[test]
    fn hevc_serde_name() {
        assert_eq!(
            serde_json::to_string(&VideoCodec::Hevc).unwrap(),
            "\"hevc\""
        );
        assert_eq!(
            serde_json::from_str::<VideoCodec>("\"hevc\"").unwrap(),
            VideoCodec::Hevc
        );
        let s = ExportSettings {
            video_codec: VideoCodec::Hevc,
            ..ExportSettings::default()
        };
        let json = serde_json::to_string(&s).unwrap();
        assert!(json.contains("\"video_codec\":\"hevc\""), "{json}");
        assert_eq!(serde_json::from_str::<ExportSettings>(&json).unwrap(), s);
    }

    #[test]
    fn validate_rejects_bad_settings() {
        let d = ExportSettings::default();
        let bad = [
            ExportSettings {
                width: 0,
                ..d.clone()
            },
            ExportSettings {
                width: 1921,
                ..d.clone()
            },
            ExportSettings {
                height: 0,
                ..d.clone()
            },
            ExportSettings {
                height: 1081,
                ..d.clone()
            },
            ExportSettings {
                fps: Rational::new(0, 1),
                ..d.clone()
            },
            ExportSettings {
                fps: Rational::new(30, 0),
                ..d.clone()
            },
            ExportSettings {
                fps: Rational::new(-30, 1),
                ..d.clone()
            },
            ExportSettings {
                audio_channels: 0,
                ..d.clone()
            },
            ExportSettings {
                audio_channels: 3,
                ..d.clone()
            },
        ];
        for s in bad {
            assert!(
                matches!(s.validate(), Err(MediaError::InvalidExportSettings(_))),
                "{s:?}"
            );
        }
    }

    #[test]
    fn export_settings_json_round_trip() {
        let s = ExportSettings::default();
        let json = serde_json::to_string(&s).unwrap();
        assert!(json.contains("\"video_codec\":\"H264\""), "{json}");
        assert_eq!(serde_json::from_str::<ExportSettings>(&json).unwrap(), s);
        let fmt: PixelFormat = serde_json::from_str("\"Rgba8\"").unwrap();
        assert_eq!(fmt, PixelFormat::Rgba8);
    }
}
