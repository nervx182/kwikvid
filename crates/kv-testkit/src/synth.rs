//! Camera-like synthetic benchmark clips (H.264 8-bit, HEVC Main10), encoded
//! in-process with `ffmpeg-next` and shared by the benches (issue #171).
//!
//! Unlike the tagged clips these carry no frame tags: the picture is built to
//! cost a decoder roughly what camera footage does (scrolling gradients, a
//! moving block and per-pixel noise, about 0.1 bit per pixel), so seek and
//! export timings are meaningful. Bytes depend on the FFmpeg build and the
//! encoder that was available; cache file names carry every [`SynthSpec`]
//! field and a format version.

use std::path::{Path, PathBuf};
use std::time::Instant;

use ffmpeg_next as ff;

use crate::cache::{fixtures_dir, try_ensure_file_in};

/// Bump when the generated content or encoder settings change, so cached
/// clips from an older run are not reused.
const SYNTH_VERSION: u32 = 1;

/// A synthetic clip to generate with [`synthetic_clip`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SynthSpec {
    pub width: u32,
    pub height: u32,
    /// Frames per second (integer rates only).
    pub fps: u32,
    pub seconds: u32,
    /// Keyframe interval in frames.
    pub gop: u32,
    /// HEVC Main10 (`libx265`, `hevc_videotoolbox`) instead of 8-bit H.264
    /// (`libx264`, `h264_videotoolbox`, `libopenh264`).
    pub ten_bit: bool,
}

/// The 6 s 2160p60 HEVC Main10 clip (GOP 60) that the seek and export
/// benches both measure, so the regression job encodes it once.
pub const HEVC10_2160P60: SynthSpec = SynthSpec {
    width: 3840,
    height: 2160,
    fps: 60,
    seconds: 6,
    gop: 60,
    ten_bit: true,
};

impl SynthSpec {
    /// Number of frames in the clip.
    pub fn frame_count(&self) -> u32 {
        self.fps * self.seconds
    }

    fn cache_key(&self) -> String {
        let codec = if self.ten_bit { "hevc10" } else { "h264" };
        format!(
            "synth-v{SYNTH_VERSION}-{codec}-{}x{}-{}fps-{}s-gop{}.mp4",
            self.width, self.height, self.fps, self.seconds, self.gop
        )
    }
}

/// Path of the clip described by `spec` in [`fixtures_dir`], encoding it on
/// first use (and printing which encoder made it, to stderr). `Err` explains
/// why no encoder could produce it, for example an FFmpeg build without a
/// 10-bit HEVC encoder.
pub fn synthetic_clip(spec: &SynthSpec) -> Result<PathBuf, String> {
    synthetic_clip_in(&fixtures_dir(), spec)
}

/// Like [`synthetic_clip`] but in an explicit directory (used by tests).
pub(crate) fn synthetic_clip_in(dir: &Path, spec: &SynthSpec) -> Result<PathBuf, String> {
    assert!(
        spec.width > 0 && spec.height > 0 && spec.fps > 0 && spec.seconds > 0 && spec.gop > 0,
        "invalid synthetic clip spec {spec:?}"
    );
    try_ensure_file_in(dir, &spec.cache_key(), |tmp| {
        let started = Instant::now();
        let encoder = encode(tmp, spec)?;
        eprintln!(
            "generated {} with {encoder} in {:.1} s",
            dir.join(spec.cache_key()).display(),
            started.elapsed().as_secs_f64()
        );
        Ok(())
    })
}

/// An encoder to try: name, input pixel format, private options.
type Candidate = (&'static str, ff::format::Pixel, Vec<(&'static str, String)>);

/// Open the first usable encoder for `s`.
fn open_encoder(
    s: &SynthSpec,
    global_header: bool,
) -> Result<(ff::Codec, ff::encoder::Video), String> {
    use ff::format::Pixel;
    let keyint = s.gop.to_string();
    let candidates: Vec<Candidate> = if s.ten_bit {
        vec![
            (
                "libx265",
                Pixel::YUV420P10LE,
                vec![
                    ("preset", "ultrafast".into()),
                    (
                        "x265-params",
                        format!("log-level=error:keyint={keyint}:min-keyint={keyint}:scenecut=0"),
                    ),
                ],
            ),
            (
                "hevc_videotoolbox",
                Pixel::P010LE,
                vec![("profile", "main10".into())],
            ),
        ]
    } else {
        vec![
            (
                "libx264",
                Pixel::YUV420P,
                vec![
                    ("preset", "ultrafast".into()),
                    (
                        "x264-params",
                        format!("keyint={keyint}:min-keyint={keyint}:scenecut=0"),
                    ),
                ],
            ),
            ("h264_videotoolbox", Pixel::YUV420P, vec![]),
            ("libopenh264", Pixel::YUV420P, vec![]),
        ]
    };
    let fps = s.fps as i32;
    let mut tried = Vec::new();
    for (name, pixel, private) in candidates {
        let Some(codec) = ff::encoder::find_by_name(name) else {
            tried.push(format!("{name}: not built in"));
            continue;
        };
        let mut enc = ff::codec::Context::new_with_codec(codec)
            .encoder()
            .video()
            .map_err(|e| format!("{name}: {e}"))?;
        enc.set_width(s.width);
        enc.set_height(s.height);
        enc.set_format(pixel);
        enc.set_time_base(ff::Rational::new(1, fps));
        enc.set_frame_rate(Some(ff::Rational::new(fps, 1)));
        enc.set_gop(s.gop);
        enc.set_colorspace(ff::color::Space::BT709);
        enc.set_color_range(ff::color::Range::MPEG);
        // Roughly camera-like: 0.1 bit per pixel (about 6 Mb/s at 1080p30,
        // 50 Mb/s at 2160p60). Only rate-controlled encoders use it.
        let pixels_per_sec = f64::from(s.width) * f64::from(s.height) * f64::from(s.fps);
        enc.set_bit_rate((pixels_per_sec * 0.1) as usize);
        if global_header {
            enc.set_flags(ff::codec::Flags::GLOBAL_HEADER);
        }
        let mut opts = ff::Dictionary::new();
        for (k, v) in &private {
            opts.set(k, v);
        }
        match enc.open_with(opts) {
            Ok(opened) => return Ok((codec, opened)),
            Err(e) => tried.push(format!("{name}: {e}")),
        }
    }
    Err(format!("no usable encoder ({})", tried.join("; ")))
}

/// Encode `s` as an MP4 at `path`; returns the encoder's name.
fn encode(path: &Path, s: &SynthSpec) -> Result<String, String> {
    ff::init().map_err(|e| e.to_string())?;
    let mut octx = ff::format::output_as(path, "mp4").map_err(|e| e.to_string())?;
    let global_header = octx
        .format()
        .flags()
        .contains(ff::format::Flags::GLOBAL_HEADER);
    let (codec, mut enc) = open_encoder(s, global_header)?;
    let fps = s.fps as i32;
    let enc_tb = ff::Rational::new(1, fps);
    {
        let mut ost = octx.add_stream(codec).map_err(|e| e.to_string())?;
        ost.set_parameters(&enc);
        ost.set_time_base(enc_tb);
        ost.set_avg_frame_rate(ff::Rational::new(fps, 1));
    }
    octx.write_header().map_err(|e| e.to_string())?;

    let drain = |enc: &mut ff::encoder::Video, octx: &mut ff::format::context::Output| {
        let mut pkt = ff::Packet::empty();
        while enc.receive_packet(&mut pkt).is_ok() {
            pkt.set_stream(0);
            pkt.rescale_ts(enc_tb, octx.stream(0).expect("stream").time_base());
            pkt.write_interleaved(octx).expect("write packet");
        }
    };
    let mut frame = ff::frame::Video::new(enc.format(), s.width, s.height);
    for i in 0..s.frame_count() {
        fill_frame(&mut frame, i);
        frame.set_pts(Some(i64::from(i)));
        enc.send_frame(&frame).map_err(|e| e.to_string())?;
        drain(&mut enc, &mut octx);
    }
    enc.send_eof().map_err(|e| e.to_string())?;
    drain(&mut enc, &mut octx);
    octx.write_trailer().map_err(|e| e.to_string())?;
    Ok(codec.name().to_string())
}

/// Draw frame `i`: diagonal gradients that scroll at different speeds, a
/// moving block and light per-pixel noise, so every frame differs and P
/// frames carry real residuals (a flat or static picture would make
/// decoding unrealistically cheap).
fn fill_frame(frame: &mut ff::frame::Video, i: u32) {
    use ff::format::Pixel;
    let (w, h) = (frame.width() as usize, frame.height() as usize);
    let format = frame.format();
    let ten_bit = matches!(format, Pixel::YUV420P10LE | Pixel::P010LE);
    let mut seed = 0x9E37_79B9_u32 ^ i.wrapping_mul(0x85EB_CA6B);
    let mut noise = move || {
        seed ^= seed << 13;
        seed ^= seed >> 17;
        seed ^= seed << 5;
        (seed & 15) as i32 - 8
    };
    let (bx, by) = ((i as usize * 13) % w, (i as usize * 7) % h);
    let luma = |x: usize, y: usize, n: i32| -> i32 {
        let mut v = ((x + y + i as usize * 4) % 512) as i32 / 2 + n;
        if x.abs_diff(bx) < w / 8 && y.abs_diff(by) < h / 8 {
            v = 235 - v / 4;
        }
        v.clamp(16, 235)
    };
    let chroma = |x: usize, y: usize, shift: usize| -> i32 {
        (64 + ((x * 2 + shift + i as usize * 3) % 256) as i32 / 2 + (y % 64) as i32).clamp(16, 240)
    };

    let put = |data: &mut [u8], o: usize, v: i32| {
        if ten_bit {
            let v = if format == Pixel::P010LE {
                (v as u16) << 8 // 10 significant bits, MSB-aligned
            } else {
                (v as u16) << 2
            };
            data[o..o + 2].copy_from_slice(&v.to_le_bytes());
        } else {
            data[o] = v as u8;
        }
    };
    let bytes = if ten_bit { 2 } else { 1 };

    let stride = frame.stride(0);
    let y_plane = frame.data_mut(0);
    for y in 0..h {
        for x in 0..w {
            put(y_plane, y * stride + x * bytes, luma(x, y, noise()));
        }
    }
    let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
    if format == Pixel::P010LE {
        let stride = frame.stride(1);
        let uv = frame.data_mut(1);
        for y in 0..ch {
            for x in 0..cw {
                put(uv, y * stride + x * 4, chroma(x, y, 0));
                put(uv, y * stride + x * 4 + 2, chroma(x, y, 97));
            }
        }
    } else {
        for (plane, shift) in [(1, 0), (2, 97)] {
            let stride = frame.stride(plane);
            let data = frame.data_mut(plane);
            for y in 0..ch {
                for x in 0..cw {
                    put(data, y * stride + x * bytes, chroma(x, y, shift));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("kv-testkit-synth-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// What a probe of the file says: codec, size, pixel format, frames.
    fn probe(path: &Path) -> (ff::codec::Id, u32, u32, ff::format::Pixel, usize) {
        let mut ictx = ff::format::input(path).expect("open clip");
        let stream = ictx
            .streams()
            .best(ff::media::Type::Video)
            .expect("video stream");
        let index = stream.index();
        let params = stream.parameters();
        let id = params.id();
        let dec = ff::codec::Context::from_parameters(params)
            .and_then(|c| c.decoder().video())
            .expect("video decoder");
        let packets = ictx.packets().filter(|(s, _)| s.index() == index).count();
        (id, dec.width(), dec.height(), dec.format(), packets)
    }

    fn tiny(ten_bit: bool) -> SynthSpec {
        SynthSpec {
            width: 64,
            height: 64,
            fps: 10,
            seconds: 1,
            gop: 5,
            ten_bit,
        }
    }

    #[test]
    fn cache_key_covers_every_field() {
        let base = tiny(false);
        let variants = [
            SynthSpec { width: 32, ..base },
            SynthSpec { height: 32, ..base },
            SynthSpec { fps: 30, ..base },
            SynthSpec { seconds: 2, ..base },
            SynthSpec { gop: 10, ..base },
            SynthSpec {
                ten_bit: true,
                ..base
            },
        ];
        for v in variants {
            assert_ne!(v.cache_key(), base.cache_key(), "{v:?}");
        }
        assert!(base.cache_key().contains(&format!("v{SYNTH_VERSION}")));
    }

    #[test]
    fn h264_clip_probes_as_spec_and_is_cached() {
        ff::init().unwrap();
        let dir = temp_dir();
        let spec = tiny(false);
        let path = synthetic_clip_in(&dir, &spec).expect("an 8-bit H.264 encoder is available");
        let (id, w, h, pixel, frames) = probe(&path);
        assert_eq!(id, ff::codec::Id::H264);
        assert_eq!((w, h), (64, 64));
        assert_eq!(pixel, ff::format::Pixel::YUV420P);
        assert_eq!(frames, 10);

        let modified = std::fs::metadata(&path).unwrap().modified().unwrap();
        let again = synthetic_clip_in(&dir, &spec).unwrap();
        assert_eq!(again, path);
        assert_eq!(
            std::fs::metadata(&again).unwrap().modified().unwrap(),
            modified,
            "a cached clip must not be re-encoded"
        );
    }

    #[test]
    fn hevc_main10_clip_probes_as_spec() {
        ff::init().unwrap();
        let dir = temp_dir();
        let path = match synthetic_clip_in(&dir, &tiny(true)) {
            Ok(path) => path,
            Err(e) => {
                eprintln!("SKIPPED: hevc_main10_clip_probes_as_spec: {e}");
                // Nothing (not even a temporary file) is left behind.
                assert!(!dir.join(tiny(true).cache_key()).exists());
                return;
            }
        };
        let (id, w, h, pixel, frames) = probe(&path);
        assert_eq!(id, ff::codec::Id::HEVC);
        assert_eq!((w, h), (64, 64));
        assert_eq!(pixel, ff::format::Pixel::YUV420P10LE);
        assert_eq!(frames, 10);
    }
}
