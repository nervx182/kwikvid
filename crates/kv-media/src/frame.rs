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
}

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
    /// Check the settings can be encoded. Rejects a zero or odd width or
    /// height (H.264 4:2:0 needs even sizes), a zero, negative or undefined
    /// frame rate, and channel counts outside 1..=2.
    pub fn validate(&self) -> Result<()> {
        let invalid = |msg: String| Err(MediaError::InvalidExportSettings(msg));
        for (name, v) in [("width", self.width), ("height", self.height)] {
            if v == 0 || v % 2 != 0 {
                return invalid(format!("{name} must be even and non-zero, got {v}"));
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
