//! Tagged MP4 clip fixtures, encoded in-process with `ffmpeg-next`.
//!
//! Every frame `i` of clip `c` shows [`FrameTag { clip: c, frame: i }`](FrameTag)
//! (see [`render_tagged_frame`]), so a test that decodes an edited export can
//! tell which source frame each output frame came from.
//!
//! Unlike the audio fixtures these files are **not** byte-identical across
//! machines: the bytes depend on the FFmpeg version and on which encoder is
//! available (`libx264`, then `libopenh264`, then FFmpeg's native `mpeg4`, so
//! LGPL builds work too). What is stable is the decoded content: every frame
//! reads back as its tag. Cache file names carry every [`ClipSpec`] field and
//! a format version.

// FFmpeg fields `ffmpeg-next` has no setter for (audio channel layout, audio
// frame buffers) are written through pointers owned by live `ffmpeg-next`
// wrappers; see each `unsafe` block.
#![allow(unsafe_code)]

use std::path::{Path, PathBuf};

use ff::ffi;
use ffmpeg_next as ff;

use crate::cache::{ensure_file_in, fixtures_dir, name_num};
use crate::tag::{read_frame_tag, render_tagged_frame, FrameTag};
use crate::yuv::{rgba_to_yuv420p, yuv420p_to_rgba, Yuv420p};

/// Bumped whenever the generated content changes.
const FORMAT_VERSION: u32 = 1;
/// Video encoders to try, in order.
const ENCODERS: [&str; 3] = ["libx264", "libopenh264", "mpeg4"];
const AUDIO_RATE: i32 = 48_000;
/// Creation time of clip 0: 2024-01-01T12:00:00Z. Clip `c` is `c` minutes
/// later.
const BASE_CREATION_UNIX: i64 = 1_704_110_400;

/// Parameters of a tagged clip.
#[derive(Debug, Clone, PartialEq)]
pub struct ClipSpec {
    /// Clip index: the tag's `clip`, the background hue and the creation time
    /// (2024-01-01T12:00:00Z plus `clip` minutes).
    pub clip: u8,
    /// Duration in seconds (> 0); the clip has `round(seconds * fps)` frames.
    pub seconds: f64,
    /// Frame width in pixels (even, >= 16).
    pub width: u32,
    /// Frame height in pixels (even, >= 16).
    pub height: u32,
    /// Frame rate as `(numerator, denominator)`, e.g. `(30000, 1001)`.
    pub fps: (i32, i32),
    /// Add a quiet 440 Hz mono AAC track.
    pub audio: bool,
    /// File name to use. `None`: a name derived from the spec, directly in
    /// [`fixtures_dir`]. `Some(name)`: `name` inside a directory derived from
    /// the spec, so the cache key still covers every field.
    pub file_name: Option<String>,
}

impl Default for ClipSpec {
    /// 320x240, 30 fps, 2 s, clip 0, no audio.
    fn default() -> Self {
        Self {
            clip: 0,
            seconds: 2.0,
            width: 320,
            height: 240,
            fps: (30, 1),
            audio: false,
            file_name: None,
        }
    }
}

impl ClipSpec {
    /// Number of video frames in the clip.
    pub fn frame_count(&self) -> usize {
        (self.seconds * f64::from(self.fps.0) / f64::from(self.fps.1)).round() as usize
    }

    fn cache_key(&self) -> String {
        format!(
            "tagged-v{FORMAT_VERSION}-clip{}-{}x{}-{}_{}fps-{}s-{}",
            self.clip,
            self.width,
            self.height,
            self.fps.0,
            self.fps.1,
            name_num(self.seconds),
            if self.audio { "aac" } else { "noaudio" },
        )
    }

    fn check(&self) {
        assert!(
            self.seconds.is_finite() && self.seconds > 0.0,
            "seconds must be finite and > 0, got {}",
            self.seconds
        );
        assert!(
            self.width >= 16 && self.height >= 16 && self.width % 2 == 0 && self.height % 2 == 0,
            "width and height must be even and >= 16, got {}x{}",
            self.width,
            self.height
        );
        assert!(
            self.fps.0 > 0 && self.fps.1 > 0,
            "fps must be positive, got {:?}",
            self.fps
        );
        let n = self.frame_count();
        assert!(
            (1..=usize::from(u16::MAX) + 1).contains(&n),
            "a clip needs 1..=65536 frames, got {n}"
        );
        if let Some(name) = &self.file_name {
            assert!(
                !name.is_empty() && !name.contains(['/', '\\']) && name != "." && name != "..",
                "file_name must be a plain file name, got {name:?}"
            );
        }
    }
}

/// Path of the tagged clip described by `spec`, encoding it on first use.
///
/// # Panics
/// On an invalid spec, if FFmpeg has none of the encoders, or on I/O failure.
pub fn tagged_clip(spec: &ClipSpec) -> PathBuf {
    spec.check();
    let key = spec.cache_key();
    let (dir, name) = match &spec.file_name {
        None => (fixtures_dir(), format!("{key}.mp4")),
        Some(name) => (fixtures_dir().join(key), name.clone()),
    };
    ensure_file_in(&dir, &name, |tmp| {
        write_tagged_clip(tmp, spec, &ENCODERS);
    })
}

/// `n` tagged clips (clip indices `0..n`), `seconds` long, at the default
/// size and rate, in one directory, named like GoPro files: `GX010001.MP4`
/// to `GX01nnnn.MP4`. Creation times are one minute apart, in file order.
///
/// # Panics
/// If `n` is 0 or above 256, or as [`tagged_clip`].
pub fn tagged_clip_set(n: usize, seconds: f64) -> Vec<PathBuf> {
    assert!(
        (1..=256).contains(&n),
        "a clip set holds 1..=256 clips, got {n}"
    );
    let dir = fixtures_dir().join(format!(
        "tagged-set-v{FORMAT_VERSION}-n{n}-{}s",
        name_num(seconds)
    ));
    (0..n)
        .map(|i| {
            let spec = ClipSpec {
                clip: i as u8,
                seconds,
                ..ClipSpec::default()
            };
            spec.check();
            ensure_file_in(&dir, &format!("GX01{:04}.MP4", i + 1), |tmp| {
                write_tagged_clip(tmp, &spec, &ENCODERS);
            })
        })
        .collect()
}

/// Decode the best video stream of `path` and read the tag of every frame,
/// in presentation order (`None` for a frame without a valid tag, e.g. a
/// crossfade).
///
/// # Panics
/// If the file cannot be opened or decoded, or decodes to a pixel format
/// other than 8-bit YUV 4:2:0.
pub fn read_clip_tags(path: &Path) -> Vec<Option<FrameTag>> {
    init();
    let mut ictx =
        ff::format::input(path).unwrap_or_else(|e| panic!("cannot open {}: {e}", path.display()));
    let stream = ictx
        .streams()
        .best(ff::media::Type::Video)
        .unwrap_or_else(|| panic!("{} has no video stream", path.display()));
    let index = stream.index();
    let mut decoder = ff::codec::context::Context::from_parameters(stream.parameters())
        .and_then(|c| c.decoder().video())
        .unwrap_or_else(|e| panic!("cannot open a decoder for {}: {e}", path.display()));

    let mut tags = Vec::new();
    let mut frame = ff::frame::Video::empty();
    let mut receive = |decoder: &mut ff::decoder::Video, tags: &mut Vec<_>| {
        while decoder.receive_frame(&mut frame).is_ok() {
            tags.push(read_frame_tag(
                &frame_to_rgba(&frame),
                frame.width(),
                frame.height(),
            ));
        }
    };
    for (s, packet) in ictx.packets() {
        if s.index() == index {
            decoder.send_packet(&packet).expect("decode packet");
            receive(&mut decoder, &mut tags);
        }
    }
    decoder.send_eof().expect("flush decoder");
    receive(&mut decoder, &mut tags);
    tags
}

fn frame_to_rgba(frame: &ff::frame::Video) -> Vec<u8> {
    use ff::format::Pixel;
    assert!(
        matches!(frame.format(), Pixel::YUV420P),
        "read_clip_tags handles 8-bit YUV 4:2:0 only, got {:?}",
        frame.format()
    );
    let (w, h) = (frame.width(), frame.height());
    let plane = |i: usize, pw: usize, ph: usize| {
        let stride = frame.stride(i);
        let data = frame.data(i);
        (0..ph)
            .flat_map(|y| data[y * stride..y * stride + pw].iter().copied())
            .collect()
    };
    let (cw, ch) = (w.div_ceil(2) as usize, h.div_ceil(2) as usize);
    yuv420p_to_rgba(&Yuv420p {
        width: w,
        height: h,
        y: plane(0, w as usize, h as usize),
        u: plane(1, cw, ch),
        v: plane(2, cw, ch),
    })
}

fn open_video_encoder(
    spec: &ClipSpec,
    encoders: &[&str],
    global_header: bool,
) -> (ff::Codec, ff::encoder::Video) {
    let mut tried = Vec::new();
    for &name in encoders {
        let Some(codec) = ff::encoder::find_by_name(name) else {
            tried.push(format!("{name}: not built in"));
            continue;
        };
        let mut enc = ff::codec::Context::new_with_codec(codec)
            .encoder()
            .video()
            .expect("video encoder context");
        enc.set_width(spec.width);
        enc.set_height(spec.height);
        enc.set_format(ff::format::Pixel::YUV420P);
        enc.set_time_base(ff::Rational::new(spec.fps.1, spec.fps.0));
        enc.set_frame_rate(Some(ff::Rational::new(spec.fps.0, spec.fps.1)));
        enc.set_gop((spec.fps.0 / spec.fps.1).max(1) as u32);
        enc.set_max_b_frames(0);
        enc.set_colorspace(ff::color::Space::BT709);
        enc.set_color_range(ff::color::Range::MPEG);
        // Generous for flat synthetic frames; only used by rate-controlled
        // encoders (mpeg4, openh264).
        let pixels_per_sec = f64::from(spec.width) * f64::from(spec.height) * f64::from(spec.fps.0)
            / f64::from(spec.fps.1);
        enc.set_bit_rate((pixels_per_sec * 0.2).max(400_000.0) as usize);
        if global_header {
            enc.set_flags(ff::codec::Flags::GLOBAL_HEADER);
        }
        let mut opts = ff::Dictionary::new();
        if name == "libx264" {
            opts.set("preset", "ultrafast");
        }
        match enc.open_with(opts) {
            Ok(opened) => return (codec, opened),
            Err(e) => tried.push(format!("{name}: {e}")),
        }
    }
    panic!(
        "no usable video encoder for tagged clips; install an FFmpeg with one of them ({})",
        tried.join("; ")
    );
}

fn drain(
    enc: &mut ff::encoder::Encoder,
    octx: &mut ff::format::context::Output,
    stream: usize,
    enc_tb: ff::Rational,
) {
    let mut pkt = ff::Packet::empty();
    while enc.receive_packet(&mut pkt).is_ok() {
        let st_tb = octx.stream(stream).expect("stream").time_base();
        pkt.set_stream(stream);
        pkt.rescale_ts(enc_tb, st_tb);
        pkt.write_interleaved(octx).expect("write packet");
    }
}

/// Encode `spec` as an MP4 at `path` with the first usable encoder of
/// `encoders`; returns the encoder's name.
fn write_tagged_clip(path: &Path, spec: &ClipSpec, encoders: &[&str]) -> String {
    init();
    let mut octx = ff::format::output_as(path, "mp4")
        .unwrap_or_else(|e| panic!("cannot create {}: {e}", path.display()));
    let global_header = octx
        .format()
        .flags()
        .contains(ff::format::Flags::GLOBAL_HEADER);

    let mut md = ff::Dictionary::new();
    md.set("creation_time", &creation_time(spec.clip));
    octx.set_metadata(md);

    let (vcodec, mut venc) = open_video_encoder(spec, encoders, global_header);
    let encoder_name = vcodec.name().to_string();
    let vtb = ff::Rational::new(spec.fps.1, spec.fps.0);
    let vidx = {
        let mut ost = octx.add_stream(vcodec).expect("add video stream");
        ost.set_parameters(&venc);
        ost.set_time_base(vtb);
        ost.set_avg_frame_rate(ff::Rational::new(spec.fps.0, spec.fps.1));
        ost.index()
    };

    let mut audio = spec.audio.then(|| {
        let codec = ff::encoder::find(ff::codec::Id::AAC).expect("aac encoder");
        let mut enc = ff::codec::Context::new_with_codec(codec)
            .encoder()
            .audio()
            .expect("audio encoder context");
        enc.set_rate(AUDIO_RATE);
        enc.set_format(ff::format::Sample::F32(ff::format::sample::Type::Planar));
        enc.set_time_base(ff::Rational::new(1, AUDIO_RATE));
        enc.set_bit_rate(64_000);
        // SAFETY: `enc` owns a valid AVCodecContext.
        unsafe { ffi::av_channel_layout_default(&mut (*enc.as_mut_ptr()).ch_layout, 1) };
        if global_header {
            enc.set_flags(ff::codec::Flags::GLOBAL_HEADER);
        }
        let enc = enc.open().expect("open aac encoder");
        let mut ost = octx.add_stream(codec).expect("add audio stream");
        ost.set_parameters(&enc);
        ost.set_time_base(ff::Rational::new(1, AUDIO_RATE));
        (ost.index(), enc)
    });

    octx.write_header().expect("write header");

    let frames = spec.frame_count();
    let mut frame = ff::frame::Video::new(ff::format::Pixel::YUV420P, spec.width, spec.height);
    for i in 0..frames {
        let tag = FrameTag {
            clip: spec.clip,
            frame: i as u16,
        };
        let yuv = rgba_to_yuv420p(
            &render_tagged_frame(tag, spec.width, spec.height),
            spec.width,
            spec.height,
        );
        let (cw, _) = yuv.chroma_size();
        for (p, (src, pw)) in [(&yuv.y, spec.width as usize), (&yuv.u, cw), (&yuv.v, cw)]
            .into_iter()
            .enumerate()
        {
            let stride = frame.stride(p);
            let dst = frame.data_mut(p);
            for (row, line) in src.chunks_exact(pw).enumerate() {
                dst[row * stride..row * stride + pw].copy_from_slice(line);
            }
        }
        frame.set_pts(Some(i as i64));
        venc.send_frame(&frame).expect("encode frame");
        drain(&mut venc, &mut octx, vidx, vtb);
    }
    venc.send_eof().expect("flush video encoder");
    drain(&mut venc, &mut octx, vidx, vtb);

    if let Some((aidx, enc)) = audio.as_mut() {
        let frame_size = i64::from(enc.frame_size());
        let total = (spec.seconds * f64::from(AUDIO_RATE)).round() as i64;
        let atb = ff::Rational::new(1, AUDIO_RATE);
        let mut pts = 0i64;
        while pts < total {
            let n = frame_size.min(total - pts);
            let mut f = ff::frame::Audio::empty();
            // SAFETY: `f` owns a fresh AVFrame; we set its audio fields,
            // allocate its buffer and write `n` samples into its one plane.
            unsafe {
                let p = f.as_mut_ptr();
                (*p).format = ffi::AVSampleFormat::AV_SAMPLE_FMT_FLTP as i32;
                (*p).nb_samples = n as i32;
                (*p).sample_rate = AUDIO_RATE;
                ffi::av_channel_layout_default(&mut (*p).ch_layout, 1);
                assert_eq!(ffi::av_frame_get_buffer(p, 0), 0);
                let data = (*p).data[0] as *mut f32;
                for s in 0..n {
                    let t = (pts + s) as f32 / AUDIO_RATE as f32;
                    *data.add(s as usize) = 0.05 * (t * 440.0 * std::f32::consts::TAU).sin();
                }
            }
            f.set_pts(Some(pts));
            enc.send_frame(&f).expect("encode audio");
            drain(enc, &mut octx, *aidx, atb);
            pts += n;
        }
        enc.send_eof().expect("flush audio encoder");
        drain(enc, &mut octx, *aidx, atb);
    }

    octx.write_trailer().expect("write trailer");
    encoder_name
}

fn init() {
    ff::init().expect("FFmpeg initialises");
    ff::log::set_level(ff::log::Level::Error);
}

/// ISO 8601 creation time of clip `clip`.
fn creation_time(clip: u8) -> String {
    let secs = BASE_CREATION_UNIX + 60 * i64::from(clip);
    // Days since the epoch to a civil date (Howard Hinnant's algorithm).
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.000000Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("kv-testkit-video-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn assert_all_tags(path: &Path, clip: u8, frames: usize) {
        let tags = read_clip_tags(path);
        assert_eq!(tags.len(), frames, "frame count of {}", path.display());
        for (i, t) in tags.iter().enumerate() {
            assert_eq!(
                *t,
                Some(FrameTag {
                    clip,
                    frame: i as u16
                }),
                "frame {i} of {}",
                path.display()
            );
        }
    }

    #[test]
    fn creation_times_are_one_minute_apart() {
        assert_eq!(creation_time(0), "2024-01-01T12:00:00.000000Z");
        assert_eq!(creation_time(1), "2024-01-01T12:01:00.000000Z");
        assert_eq!(creation_time(255), "2024-01-01T16:15:00.000000Z");
    }

    #[test]
    fn cache_key_covers_every_field() {
        let base = ClipSpec::default();
        let variants = [
            ClipSpec {
                clip: 1,
                ..base.clone()
            },
            ClipSpec {
                seconds: 2.5,
                ..base.clone()
            },
            ClipSpec {
                width: 640,
                ..base.clone()
            },
            ClipSpec {
                height: 480,
                ..base.clone()
            },
            ClipSpec {
                fps: (30000, 1001),
                ..base.clone()
            },
            ClipSpec {
                audio: true,
                ..base.clone()
            },
        ];
        let mut keys: Vec<_> = variants.iter().map(ClipSpec::cache_key).collect();
        keys.push(base.cache_key());
        assert!(base.cache_key().contains(&format!("v{FORMAT_VERSION}")));
        let n = keys.len();
        keys.sort();
        keys.dedup();
        assert_eq!(keys.len(), n);
    }

    #[test]
    fn native_mpeg4_fallback_round_trips_every_frame() {
        init();
        let spec = ClipSpec {
            clip: 5,
            seconds: 1.0,
            ..ClipSpec::default()
        };
        let path = temp_dir().join("mpeg4.mp4");
        let used = write_tagged_clip(&path, &spec, &["no-such-encoder", "mpeg4"]);
        assert_eq!(used, "mpeg4");
        assert_all_tags(&path, 5, 30);
    }

    #[test]
    fn default_clip_round_trips_every_frame() {
        let spec = ClipSpec {
            clip: 3,
            ..ClipSpec::default()
        };
        let path = tagged_clip(&spec);
        assert!(path.starts_with(fixtures_dir()));
        assert_all_tags(&path, 3, 60);
        // Cached: the same path, not re-encoded.
        let mtime = std::fs::metadata(&path).unwrap().modified().unwrap();
        assert_eq!(tagged_clip(&spec), path);
        assert_eq!(std::fs::metadata(&path).unwrap().modified().unwrap(), mtime);
    }

    #[test]
    fn ntsc_rate_clip_with_audio_and_custom_name() {
        let spec = ClipSpec {
            clip: 11,
            seconds: 0.5,
            width: 192,
            height: 108,
            fps: (30000, 1001),
            audio: true,
            file_name: Some("custom.MP4".into()),
        };
        let path = tagged_clip(&spec);
        assert_eq!(path.file_name().unwrap(), "custom.MP4");
        assert!(path.parent().unwrap().ends_with(spec.cache_key()));
        assert_all_tags(&path, 11, spec.frame_count());

        let ictx = ff::format::input(&path).unwrap();
        let audio = ictx.streams().best(ff::media::Type::Audio).expect("audio");
        assert_eq!(audio.parameters().id(), ff::codec::Id::AAC);
        let video = ictx.streams().best(ff::media::Type::Video).unwrap();
        assert_eq!(video.avg_frame_rate(), ff::Rational::new(30000, 1001));
    }

    #[test]
    fn clip_set_has_gopro_names_and_increasing_creation_times() {
        let paths = tagged_clip_set(3, 0.2);
        let names: Vec<_> = paths
            .iter()
            .map(|p| p.file_name().unwrap().to_str().unwrap().to_owned())
            .collect();
        assert_eq!(names, ["GX010001.MP4", "GX010002.MP4", "GX010003.MP4"]);
        assert!(paths.iter().all(|p| p.parent() == paths[0].parent()));
        let times: Vec<String> = paths
            .iter()
            .map(|p| {
                let ictx = ff::format::input(p).unwrap();
                ictx.metadata().get("creation_time").unwrap().to_owned()
            })
            .collect();
        assert_eq!(
            times,
            [
                "2024-01-01T12:00:00.000000Z",
                "2024-01-01T12:01:00.000000Z",
                "2024-01-01T12:02:00.000000Z"
            ]
        );
        for (i, p) in paths.iter().enumerate() {
            assert_all_tags(p, i as u8, 6);
        }
    }
}
