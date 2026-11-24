//! Probe tests against tiny clips encoded at test time with the FFmpeg API
//! (2 s, 320x240). Real GoPro files belong to the regression suite.
#![cfg(feature = "ffmpeg")]
// Fixture generation sets a few FFmpeg fields `ffmpeg-next` has no setter for
// (channel layout, display matrix side data); every `unsafe` block writes
// through pointers owned by a live `ffmpeg-next` wrapper.
#![allow(unsafe_code)]

use std::path::{Path, PathBuf};

use ff::ffi;
use ffmpeg_next as ff;
use kv_core::Micros;
use kv_media::{FfmpegBackend, MediaBackend, MediaError, Rational};

const WIDTH: u32 = 320;
const HEIGHT: u32 = 240;
const SECONDS: i64 = 2;

struct AudioSpec {
    rate: i32,
    channels: i32,
}

struct ClipSpec<'a> {
    /// Encoder names to try, in order, with the pixel format to feed each.
    encoders: &'a [(&'a str, ff::format::Pixel)],
    /// Frame rate as num/den.
    fps: (i32, i32),
    /// Display matrix written with `av_display_rotation_set(m, angle)`.
    display_matrix_angle: Option<f64>,
    audio: Option<AudioSpec>,
    creation_time: Option<&'a str>,
}

fn out_dir() -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("kv-media-probe");
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Open the first available encoder from `spec.encoders`.
fn open_video_encoder(
    spec: &ClipSpec,
    global_header: bool,
) -> (ff::Codec, ff::encoder::Video, ff::format::Pixel) {
    let tb = ff::Rational::new(spec.fps.1, spec.fps.0);
    let mut tried = Vec::new();
    for &(name, pix) in spec.encoders {
        let Some(codec) = ff::encoder::find_by_name(name) else {
            tried.push(format!("{name}: not built in"));
            continue;
        };
        let mut enc = ff::codec::Context::new_with_codec(codec)
            .encoder()
            .video()
            .unwrap();
        enc.set_width(WIDTH);
        enc.set_height(HEIGHT);
        enc.set_format(pix);
        enc.set_time_base(tb);
        enc.set_frame_rate(Some(ff::Rational::new(spec.fps.0, spec.fps.1)));
        enc.set_gop(30);
        if global_header {
            enc.set_flags(ff::codec::Flags::GLOBAL_HEADER);
        }
        let mut opts = ff::Dictionary::new();
        if name.starts_with("libx26") {
            opts.set("preset", "ultrafast");
        }
        if name == "libx265" {
            opts.set("x265-params", "log-level=error");
        }
        match enc.open_with(opts) {
            Ok(opened) => return (codec, opened, pix),
            Err(e) => tried.push(format!("{name}: {e}")),
        }
    }
    panic!(
        "no usable encoder for the test fixture; install an FFmpeg with one of them ({})",
        tried.join("; ")
    );
}

fn fill_video_frame(frame: &mut ff::frame::Video, i: usize) {
    let ten_bit = frame.format() == ff::format::Pixel::YUV420P10LE;
    for plane in 0..3 {
        let (w, h) = if plane == 0 {
            (WIDTH as usize, HEIGHT as usize)
        } else {
            (WIDTH as usize / 2, HEIGHT as usize / 2)
        };
        let stride = frame.stride(plane);
        let data = frame.data_mut(plane);
        for y in 0..h {
            for x in 0..w {
                let v = if plane == 0 {
                    (x + y + i * 3) % 220 + 16
                } else {
                    128
                };
                if ten_bit {
                    let o = y * stride + x * 2;
                    data[o..o + 2].copy_from_slice(&((v as u16) << 2).to_le_bytes());
                } else {
                    data[y * stride + x] = v as u8;
                }
            }
        }
    }
}

fn drain(
    enc: &mut ff::encoder::Encoder,
    octx: &mut ff::format::context::Output,
    stream: usize,
    enc_tb: ff::Rational,
) {
    let mut pkt = ff::Packet::empty();
    while enc.receive_packet(&mut pkt).is_ok() {
        let st_tb = octx.stream(stream).unwrap().time_base();
        pkt.set_stream(stream);
        pkt.rescale_ts(enc_tb, st_tb);
        pkt.write_interleaved(octx).unwrap();
    }
}

/// Encode a clip described by `spec` to `path`; returns the encoder used.
fn write_clip(path: &Path, spec: &ClipSpec) -> String {
    let mut octx = ff::format::output(path).unwrap();
    let global_header = octx
        .format()
        .flags()
        .contains(ff::format::Flags::GLOBAL_HEADER);

    if let Some(t) = spec.creation_time {
        let mut md = ff::Dictionary::new();
        md.set("creation_time", t);
        octx.set_metadata(md);
    }

    // Video stream.
    let (vcodec, mut venc, pix) = open_video_encoder(spec, global_header);
    let encoder_name = vcodec.name().to_string();
    let vtb = ff::Rational::new(spec.fps.1, spec.fps.0);
    let vidx = {
        let mut ost = octx.add_stream(vcodec).unwrap();
        ost.set_parameters(&venc);
        ost.set_time_base(vtb);
        ost.set_avg_frame_rate(ff::Rational::new(spec.fps.0, spec.fps.1));
        if let Some(angle) = spec.display_matrix_angle {
            // SAFETY: the stream and its codecpar are owned by `octx`.
            unsafe {
                let par = (*ost.as_mut_ptr()).codecpar;
                let sd = ffi::av_packet_side_data_new(
                    &mut (*par).coded_side_data,
                    &mut (*par).nb_coded_side_data,
                    ffi::AVPacketSideDataType::AV_PKT_DATA_DISPLAYMATRIX,
                    36,
                    0,
                );
                assert!(!sd.is_null());
                ffi::av_display_rotation_set((*sd).data as *mut i32, angle);
            }
        }
        ost.index()
    };

    // Audio stream (AAC, FFmpeg's native LGPL encoder).
    let mut audio = spec.audio.as_ref().map(|a| {
        let codec = ff::encoder::find(ff::codec::Id::AAC).expect("aac encoder");
        let mut enc = ff::codec::Context::new_with_codec(codec)
            .encoder()
            .audio()
            .unwrap();
        enc.set_rate(a.rate);
        enc.set_format(ff::format::Sample::F32(ff::format::sample::Type::Planar));
        enc.set_time_base(ff::Rational::new(1, a.rate));
        enc.set_bit_rate(64_000);
        // SAFETY: `enc` owns a valid AVCodecContext.
        unsafe { ffi::av_channel_layout_default(&mut (*enc.as_mut_ptr()).ch_layout, a.channels) };
        if global_header {
            enc.set_flags(ff::codec::Flags::GLOBAL_HEADER);
        }
        let enc = enc.open().unwrap();
        let mut ost = octx.add_stream(codec).unwrap();
        ost.set_parameters(&enc);
        ost.set_time_base(ff::Rational::new(1, a.rate));
        (ost.index(), enc, a)
    });

    octx.write_header().unwrap();

    // Video frames.
    let frames = (SECONDS * i64::from(spec.fps.0) / i64::from(spec.fps.1)) as usize;
    let mut frame = ff::frame::Video::new(pix, WIDTH, HEIGHT);
    for i in 0..frames {
        fill_video_frame(&mut frame, i);
        frame.set_pts(Some(i as i64));
        venc.send_frame(&frame).unwrap();
        drain(&mut venc, &mut octx, vidx, vtb);
    }
    venc.send_eof().unwrap();
    drain(&mut venc, &mut octx, vidx, vtb);

    // Audio frames: a 440 Hz sine.
    if let Some((aidx, enc, a)) = audio.as_mut() {
        let frame_size = enc.frame_size() as i64;
        let total = SECONDS * i64::from(a.rate);
        let atb = ff::Rational::new(1, a.rate);
        let mut pts = 0i64;
        while pts < total {
            let n = frame_size.min(total - pts);
            let mut f = ff::frame::Audio::empty();
            // SAFETY: `f` owns a fresh AVFrame; we set its audio fields,
            // allocate its buffers and write within `nb_samples` per plane.
            unsafe {
                let p = f.as_mut_ptr();
                (*p).format = ffi::AVSampleFormat::AV_SAMPLE_FMT_FLTP as i32;
                (*p).nb_samples = n as i32;
                (*p).sample_rate = a.rate;
                ffi::av_channel_layout_default(&mut (*p).ch_layout, a.channels);
                assert_eq!(ffi::av_frame_get_buffer(p, 0), 0);
                for ch in 0..a.channels as usize {
                    let data = (*p).data[ch] as *mut f32;
                    for s in 0..n {
                        let t = (pts + s) as f32 / a.rate as f32;
                        *data.add(s as usize) = 0.3 * (t * 440.0 * std::f32::consts::TAU).sin();
                    }
                }
            }
            f.set_pts(Some(pts));
            enc.send_frame(&f).unwrap();
            drain(enc, &mut octx, *aidx, atb);
            pts += n;
        }
        enc.send_eof().unwrap();
        drain(enc, &mut octx, *aidx, atb);
    }

    octx.write_trailer().unwrap();
    encoder_name
}

fn backend() -> FfmpegBackend {
    FfmpegBackend::new().expect("FFmpeg initialises")
}

fn assert_duration_near(actual: Micros, expected_secs: i64) {
    let diff = (actual.0 - expected_secs * 1_000_000).abs();
    assert!(
        diff < 100_000,
        "duration {actual:?} not within 100 ms of {expected_secs} s"
    );
}

#[test]
fn probes_h264_mp4_with_aac_rotation_and_creation_time() {
    ff::init().unwrap();
    let path = out_dir().join("h264_aac_rot90.mp4");
    let encoder = write_clip(
        &path,
        &ClipSpec {
            encoders: &[
                ("libx264", ff::format::Pixel::YUV420P),
                ("libopenh264", ff::format::Pixel::YUV420P),
            ],
            fps: (30000, 1001),
            // An upright phone video: ffprobe shows rotation -90.
            display_matrix_angle: Some(90.0),
            audio: Some(AudioSpec {
                rate: 48_000,
                channels: 2,
            }),
            creation_time: Some("2024-05-06T07:08:09.000000Z"),
        },
    );

    let info = backend().probe(&path).unwrap();

    assert_duration_near(info.duration, SECONDS);
    assert!(
        info.container.split(',').any(|c| c == "mp4"),
        "container {:?}",
        info.container
    );
    let v = info.video.expect("video stream");
    assert_eq!(v.codec, "h264", "encoded with {encoder}");
    assert_eq!((v.width, v.height), (WIDTH, HEIGHT));
    assert_eq!(v.fps, Rational::new(30000, 1001));
    assert_eq!(v.bit_depth, 8);
    assert_eq!(v.rotation, 90);
    let a = info.audio.expect("audio stream");
    assert_eq!(a.codec, "aac");
    assert_eq!(a.sample_rate, 48_000);
    assert_eq!(a.channels, 2);
    // 2024-05-06T07:08:09Z
    assert_eq!(info.creation_time, Some(Micros(1_714_979_289_000_000)));
}

#[test]
fn probes_10_bit_hevc_mkv_with_mono_audio_and_no_metadata() {
    ff::init().unwrap();
    let path = out_dir().join("hevc10_mono.mkv");
    write_clip(
        &path,
        &ClipSpec {
            encoders: &[("libx265", ff::format::Pixel::YUV420P10LE)],
            fps: (25, 1),
            display_matrix_angle: None,
            audio: Some(AudioSpec {
                rate: 44_100,
                channels: 1,
            }),
            creation_time: None,
        },
    );

    let info = backend().probe(&path).unwrap();

    assert_duration_near(info.duration, SECONDS);
    assert_eq!(info.container, "matroska,webm");
    let v = info.video.expect("video stream");
    assert_eq!(v.codec, "hevc");
    assert_eq!((v.width, v.height), (WIDTH, HEIGHT));
    assert_eq!(v.fps, Rational::new(25, 1));
    assert_eq!(v.bit_depth, 10);
    assert_eq!(v.rotation, 0);
    let a = info.audio.expect("audio stream");
    assert_eq!(a.codec, "aac");
    assert_eq!(a.sample_rate, 44_100);
    assert_eq!(a.channels, 1);
    assert_eq!(info.creation_time, None);
}

#[test]
fn probes_hevc_mov_without_audio() {
    ff::init().unwrap();
    let path = out_dir().join("hevc_rot180_noaudio.mov");
    write_clip(
        &path,
        &ClipSpec {
            encoders: &[("libx265", ff::format::Pixel::YUV420P)],
            fps: (30, 1),
            display_matrix_angle: Some(180.0),
            audio: None,
            creation_time: Some("2023-01-02T03:04:05Z"),
        },
    );

    let info = backend().probe(&path).unwrap();

    assert_duration_near(info.duration, SECONDS);
    assert!(
        info.container.split(',').any(|c| c == "mov"),
        "container {:?}",
        info.container
    );
    let v = info.video.expect("video stream");
    assert_eq!(v.codec, "hevc");
    assert_eq!((v.width, v.height), (WIDTH, HEIGHT));
    assert_eq!(v.fps, Rational::new(30, 1));
    assert_eq!(v.bit_depth, 8);
    assert_eq!(v.rotation, 180);
    assert_eq!(info.audio, None);
    assert_eq!(info.creation_time, Some(Micros(1_672_628_645_000_000)));
}

#[test]
fn missing_file_is_an_open_error() {
    let path = out_dir().join("does-not-exist.mp4");
    match backend().probe(&path) {
        Err(MediaError::Open { path: p, .. }) => assert_eq!(p, path),
        other => panic!("expected Open error, got {other:?}"),
    }
}

#[test]
fn non_media_file_is_an_open_error() {
    let path = out_dir().join("not-media.mp4");
    std::fs::write(&path, b"definitely not a video").unwrap();
    assert!(matches!(
        backend().probe(&path),
        Err(MediaError::Open { .. })
    ));
}
