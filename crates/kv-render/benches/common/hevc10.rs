//! Generated 2160p60 HEVC Main10 source clip shared by the kv-render
//! benches (`export`, `preview`), as in `crates/kv-media/benches/seek.rs`:
//! `libx265`, or `hevc_videotoolbox` when the FFmpeg build has no libx265.

use std::path::{Path, PathBuf};
use std::time::Instant;

use ffmpeg_next as ff;

/// Length of the generated clip.
pub const CLIP_SECONDS: f64 = 6.0;

const HEVC_WIDTH: u32 = 3840;
const HEVC_HEIGHT: u32 = 2160;
const HEVC_FPS: i32 = 60;
const HEVC_GOP: u32 = 60;
/// Bump when the generated content or encoder settings change.
const HEVC_FIXTURE_VERSION: u32 = 1;

/// Path of the HEVC Main10 clip in `dir`, encoding it on first use. `Err`
/// explains why no encoder could produce it.
pub fn hevc10_clip(dir: &Path) -> Result<PathBuf, String> {
    let path = dir.join(format!("hevc10-2160p60-gop60-v{HEVC_FIXTURE_VERSION}.mp4"));
    if path.exists() {
        return Ok(path);
    }
    let tmp = path.with_extension("mp4.tmp");
    let started = Instant::now();
    let encoder = encode_hevc10(&tmp)?;
    std::fs::rename(&tmp, &path).expect("move fixture into place");
    eprintln!(
        "generated {} with {encoder} in {:.1} s",
        path.display(),
        started.elapsed().as_secs_f64()
    );
    Ok(path)
}

/// An encoder to try: name, input pixel format, private options.
type Candidate = (&'static str, ff::format::Pixel, Vec<(&'static str, String)>);

/// Open the first usable 10-bit HEVC encoder.
fn open_hevc_encoder(global_header: bool) -> Result<(ff::Codec, ff::encoder::Video), String> {
    use ff::format::Pixel;
    let keyint = HEVC_GOP.to_string();
    let candidates: [Candidate; 2] = [
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
    ];
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
        enc.set_width(HEVC_WIDTH);
        enc.set_height(HEVC_HEIGHT);
        enc.set_format(pixel);
        enc.set_time_base(ff::Rational::new(1, HEVC_FPS));
        enc.set_frame_rate(Some(ff::Rational::new(HEVC_FPS, 1)));
        enc.set_gop(HEVC_GOP);
        enc.set_colorspace(ff::color::Space::BT709);
        enc.set_color_range(ff::color::Range::MPEG);
        // 0.1 bit per pixel, about 50 Mb/s: roughly camera-like.
        let pixels_per_sec = f64::from(HEVC_WIDTH) * f64::from(HEVC_HEIGHT) * f64::from(HEVC_FPS);
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

/// Encode the HEVC clip at `path`; returns the encoder's name.
fn encode_hevc10(path: &Path) -> Result<String, String> {
    ff::init().map_err(|e| e.to_string())?;
    let mut octx = ff::format::output_as(path, "mp4").map_err(|e| e.to_string())?;
    let global_header = octx
        .format()
        .flags()
        .contains(ff::format::Flags::GLOBAL_HEADER);
    let (codec, mut enc) = open_hevc_encoder(global_header)?;
    let enc_tb = ff::Rational::new(1, HEVC_FPS);
    {
        let mut ost = octx.add_stream(codec).map_err(|e| e.to_string())?;
        ost.set_parameters(&enc);
        ost.set_time_base(enc_tb);
        ost.set_avg_frame_rate(ff::Rational::new(HEVC_FPS, 1));
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
    let mut frame = ff::frame::Video::new(enc.format(), HEVC_WIDTH, HEVC_HEIGHT);
    let frames = (f64::from(HEVC_FPS) * CLIP_SECONDS) as i32;
    for i in 0..frames {
        fill_frame(&mut frame, i as u32);
        frame.set_pts(Some(i64::from(i)));
        enc.send_frame(&frame).map_err(|e| e.to_string())?;
        drain(&mut enc, &mut octx);
    }
    enc.send_eof().map_err(|e| e.to_string())?;
    drain(&mut enc, &mut octx);
    octx.write_trailer().map_err(|e| e.to_string())?;
    Ok(codec.name().to_string())
}

/// Draw frame `i` of a 10-bit picture: scrolling diagonal gradients, a moving
/// block and light noise, so P frames carry real residuals.
fn fill_frame(frame: &mut ff::frame::Video, i: u32) {
    use ff::format::Pixel;
    let (w, h) = (frame.width() as usize, frame.height() as usize);
    let p010 = frame.format() == Pixel::P010LE;
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
        // 8-bit value scaled to 10 bits; P010 is MSB-aligned.
        let v = if p010 {
            (v as u16) << 8
        } else {
            (v as u16) << 2
        };
        data[o..o + 2].copy_from_slice(&v.to_le_bytes());
    };

    let stride = frame.stride(0);
    let y_plane = frame.data_mut(0);
    for y in 0..h {
        for x in 0..w {
            put(y_plane, y * stride + x * 2, luma(x, y, noise()));
        }
    }
    let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
    if p010 {
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
                    put(data, y * stride + x * 2, chroma(x, y, shift));
                }
            }
        }
    }
}
