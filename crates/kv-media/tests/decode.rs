//! Decode tests against tagged fixture clips from `kv-testkit`: every frame
//! shows its clip and frame index, so a decoded frame can be identified.
//! Real GoPro files (5.3K HEVC, rotated) belong to the regression suite.
#![cfg(feature = "ffmpeg")]
// The rotation and 10-bit fixtures set FFmpeg fields `ffmpeg-next` has no
// setter for (display matrix side data, 10-bit planes); every `unsafe` block
// writes through pointers owned by a live `ffmpeg-next` wrapper.
#![allow(unsafe_code)]

use std::path::{Path, PathBuf};

use ff::ffi;
use ffmpeg_next as ff;
use kv_core::Micros;
use kv_media::{DecodeOptions, FfmpegBackend, MediaBackend, MediaError, VideoReader};
use kv_testkit::{
    read_frame_tag, render_tagged_frame, rgba_to_yuv420p, tagged_clip, ClipSpec, FrameTag,
};

fn backend() -> FfmpegBackend {
    FfmpegBackend::new().expect("FFmpeg initialises")
}

fn out_dir() -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("kv-media-decode");
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// 2 s, 320x240, 30 fps tagged clip 3.
fn clip_spec() -> ClipSpec {
    ClipSpec {
        clip: 3,
        ..ClipSpec::default()
    }
}

fn frame_us(i: usize, fps: (i32, i32)) -> i64 {
    (i as i64 * 1_000_000 * i64::from(fps.1) + i64::from(fps.0) / 2) / i64::from(fps.0)
}

fn tag_of(f: &kv_media::VideoFrame) -> Option<FrameTag> {
    read_frame_tag(&f.data, f.width, f.height)
}

#[test]
fn every_frame_decodes_in_order_with_tag_and_pts() {
    let spec = clip_spec();
    let path = tagged_clip(&spec);
    let mut r = backend()
        .open_video(&path, &DecodeOptions::default())
        .unwrap();
    assert_eq!((r.info().width, r.info().height), (320, 240));

    let half_frame = frame_us(1, spec.fps) / 2;
    let mut n = 0;
    while let Some(f) = r.next_frame().unwrap() {
        assert_eq!((f.width, f.height), (320, 240));
        assert_eq!(
            tag_of(&f),
            Some(FrameTag {
                clip: 3,
                frame: n as u16
            }),
            "frame {n}"
        );
        let want = frame_us(n, spec.fps);
        assert!(
            (f.pts.0 - want).abs() <= half_frame,
            "frame {n}: pts {:?}, expected about {want} us",
            f.pts
        );
        n += 1;
    }
    assert_eq!(n, spec.frame_count());
    assert!(r.next_frame().unwrap().is_none(), "stays at the end");
}

#[test]
fn frame_at_random_times_forwards_and_backwards() {
    let spec = ClipSpec {
        clip: 5,
        seconds: 6.0,
        ..ClipSpec::default()
    };
    let path = tagged_clip(&spec);
    let mut r = backend()
        .open_video_reader(&path, &DecodeOptions::default())
        .unwrap();
    let frames = spec.frame_count();
    let expect = |t: i64| -> u16 {
        // Largest frame index whose pts <= t (pts rounded to the nearest us).
        (0..frames)
            .rev()
            .find(|&i| frame_us(i, spec.fps) <= t)
            .unwrap() as u16
    };

    // Deterministic pseudo-random times over the clip and a bit past it.
    let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
    for _ in 0..40 {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        let t = (x % 6_500_000) as i64;
        let f = r.frame_at(Micros(t)).unwrap().expect("a frame at t");
        assert_eq!(tag_of(&f).map(|g| g.frame), Some(expect(t)), "t = {t} us");
        assert!(f.pts.0 <= t);
    }
    // Frame boundaries exactly.
    for i in [0usize, 1, 59, 60, 61, frames - 1] {
        let t = frame_us(i, spec.fps);
        let f = r.frame_at(Micros(t)).unwrap().unwrap();
        assert_eq!(tag_of(&f).map(|g| g.frame), Some(i as u16), "boundary {i}");
        let f = r.frame_at(Micros(t - 1)).unwrap();
        if i == 0 {
            assert!(f.is_none(), "nothing is displayed before the first frame");
        } else {
            assert_eq!(tag_of(&f.unwrap()).map(|g| g.frame), Some(i as u16 - 1));
        }
    }
}

#[test]
fn sequential_frame_at_does_not_reseek() {
    let spec = ClipSpec {
        clip: 1,
        seconds: 4.0,
        ..ClipSpec::default()
    };
    let path = tagged_clip(&spec);
    let mut r = backend()
        .open_video_reader(&path, &DecodeOptions::default())
        .unwrap();

    // 60 fps output over a 30 fps clip: every source frame is asked for twice.
    let mut t = 0;
    let mut last = 0;
    while t < 4_000_000 {
        let f = r.frame_at(Micros(t)).unwrap().unwrap();
        let frame = tag_of(&f).unwrap().frame;
        assert!(frame >= last);
        last = frame;
        t += 16_667;
    }
    assert_eq!(last as usize, spec.frame_count() - 1);
    assert_eq!(
        r.seek_count(),
        0,
        "increasing frame_at calls decode forward"
    );

    // A jump back seeks once and then plays on without seeking.
    r.frame_at(Micros(1_000_000)).unwrap();
    for i in 0..30 {
        r.frame_at(Micros(1_000_000 + i * 33_333)).unwrap();
    }
    assert_eq!(r.seek_count(), 1);
}

#[test]
fn seek_then_next_frame() {
    let path = tagged_clip(&ClipSpec {
        clip: 2,
        seconds: 4.0,
        ..ClipSpec::default()
    });
    let mut r = backend()
        .open_video(&path, &DecodeOptions::default())
        .unwrap();
    for (t, want) in [(3_000_000, 90u16), (1_010_000, 30), (0, 0), (2_999_999, 89)] {
        r.seek(Micros(t)).unwrap();
        let f = r.next_frame().unwrap().unwrap();
        assert_eq!(tag_of(&f).map(|g| g.frame), Some(want), "seek {t}");
        let f = r.next_frame().unwrap().unwrap();
        assert_eq!(
            tag_of(&f).map(|g| g.frame),
            Some(want + 1),
            "after seek {t}"
        );
    }
}

#[test]
fn scaling_to_160x90_keeps_tags() {
    // 16:9 source, so the scale keeps the aspect and the square code stays
    // square (the tag survives scaling, not anisotropic squashing).
    let path = tagged_clip(&ClipSpec {
        width: 640,
        height: 360,
        ..clip_spec()
    });
    let opts = DecodeOptions {
        size: Some((160, 90)),
        apply_rotation: true,
    };
    let mut r = backend().open_video(&path, &opts).unwrap();
    let mut n = 0u16;
    while let Some(f) = r.next_frame().unwrap() {
        assert_eq!((f.width, f.height, f.data.len()), (160, 90, 160 * 90 * 4));
        assert_eq!(tag_of(&f), Some(FrameTag { clip: 3, frame: n }));
        n += 1;
    }
    assert_eq!(n, 60);
}

#[test]
fn truncated_file_is_an_error_not_a_panic() {
    let src = tagged_clip(&clip_spec());
    let bytes = std::fs::read(&src).unwrap();
    for (name, len) in [("trunc-half.mp4", bytes.len() / 2), ("trunc-head.mp4", 64)] {
        let path = out_dir().join(name);
        std::fs::write(&path, &bytes[..len]).unwrap();
        let result = backend()
            .open_video(&path, &DecodeOptions::default())
            .and_then(|mut r| {
                while r.next_frame()?.is_some() {}
                Ok(())
            });
        assert!(
            matches!(
                result,
                Err(MediaError::Open { .. } | MediaError::Decode { .. })
            ),
            "{name}: {result:?}"
        );
    }
}

#[test]
fn audio_only_file_has_no_video_stream() {
    let path = out_dir().join("audio-only.wav");
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: 8000,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut w = hound::WavWriter::create(&path, spec).unwrap();
    for _ in 0..800 {
        w.write_sample(0i16).unwrap();
    }
    w.finalize().unwrap();
    assert!(matches!(
        backend().open_video(&path, &DecodeOptions::default()),
        Err(MediaError::NoVideoStream(_))
    ));
}

/// Remux `src` into `dst`, adding a display matrix written with
/// `av_display_rotation_set(m, angle)` to the video stream.
fn remux_with_rotation(src: &Path, dst: &Path, angle: f64) {
    let mut ictx = ff::format::input(src).unwrap();
    let mut octx = ff::format::output(dst).unwrap();
    let ist = ictx.streams().best(ff::media::Type::Video).unwrap();
    let in_index = ist.index();
    let in_tb = ist.time_base();
    {
        let mut ost = octx
            .add_stream(ff::encoder::find(ff::codec::Id::None))
            .unwrap();
        ost.set_parameters(ist.parameters());
        ost.set_time_base(in_tb);
        // SAFETY: the stream and its codecpar are owned by `octx`.
        unsafe {
            let par = (*ost.as_mut_ptr()).codecpar;
            (*par).codec_tag = 0;
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
    octx.write_header().unwrap();
    let out_tb = octx.stream(0).unwrap().time_base();
    for (s, mut packet) in ictx.packets() {
        if s.index() == in_index {
            packet.rescale_ts(in_tb, out_tb);
            packet.set_position(-1);
            packet.set_stream(0);
            packet.write_interleaved(&mut octx).unwrap();
        }
    }
    octx.write_trailer().unwrap();
}

fn rotate_cw(data: &[u8], w: u32, h: u32) -> Vec<u8> {
    let (w, h) = (w as usize, h as usize);
    let mut out = Vec::with_capacity(data.len());
    for y in 0..w {
        for x in 0..h {
            let o = ((h - 1 - x) * w + y) * 4;
            out.extend_from_slice(&data[o..o + 4]);
        }
    }
    out
}

#[test]
fn rotation_is_applied_before_scaling() {
    let src = tagged_clip(&clip_spec());
    let path = out_dir().join("rot90.mp4");
    // An upright phone video: needs 90 degrees clockwise.
    remux_with_rotation(&src, &path, 90.0);
    let b = backend();

    let mut plain = b
        .open_video(
            &path,
            &DecodeOptions {
                size: None,
                apply_rotation: false,
            },
        )
        .unwrap();
    assert_eq!(plain.info().rotation, 90);
    let p = plain.next_frame().unwrap().unwrap();
    assert_eq!((p.width, p.height), (320, 240));
    assert_eq!(tag_of(&p), Some(FrameTag { clip: 3, frame: 0 }));

    let mut upright = b.open_video(&path, &DecodeOptions::default()).unwrap();
    let u = upright.next_frame().unwrap().unwrap();
    assert_eq!((u.width, u.height), (240, 320), "display size");
    assert_eq!(u.data, rotate_cw(&p.data, 320, 240));

    // A requested size is the final (upright) size.
    let mut scaled = b
        .open_video(
            &path,
            &DecodeOptions {
                size: Some((90, 160)),
                apply_rotation: true,
            },
        )
        .unwrap();
    let s = scaled.next_frame().unwrap().unwrap();
    assert_eq!((s.width, s.height), (90, 160));
}

/// Encode a 1 s, 25 fps, 320x240 tagged clip as 10-bit HEVC (clip 7).
/// Returns `None` when `libx265` is unavailable.
fn write_hevc10(path: &Path) -> Option<usize> {
    const W: u32 = 320;
    const H: u32 = 240;
    const FRAMES: usize = 25;
    ff::init().unwrap();
    let codec = ff::encoder::find_by_name("libx265")?;
    let mut octx = ff::format::output(path).unwrap();
    let global = octx
        .format()
        .flags()
        .contains(ff::format::Flags::GLOBAL_HEADER);
    let tb = ff::Rational::new(1, 25);
    let mut enc = ff::codec::Context::new_with_codec(codec)
        .encoder()
        .video()
        .unwrap();
    enc.set_width(W);
    enc.set_height(H);
    enc.set_format(ff::format::Pixel::YUV420P10LE);
    enc.set_time_base(tb);
    enc.set_frame_rate(Some(ff::Rational::new(25, 1)));
    enc.set_colorspace(ff::color::Space::BT709);
    enc.set_color_range(ff::color::Range::MPEG);
    if global {
        enc.set_flags(ff::codec::Flags::GLOBAL_HEADER);
    }
    let mut opts = ff::Dictionary::new();
    opts.set("preset", "ultrafast");
    opts.set("x265-params", "log-level=error");
    let Ok(mut enc) = enc.open_with(opts) else {
        return None;
    };
    {
        let mut ost = octx.add_stream(codec).unwrap();
        ost.set_parameters(&enc);
        ost.set_time_base(tb);
    }
    octx.write_header().unwrap();
    let drain = |enc: &mut ff::encoder::Video, octx: &mut ff::format::context::Output| {
        let mut pkt = ff::Packet::empty();
        while enc.receive_packet(&mut pkt).is_ok() {
            pkt.set_stream(0);
            pkt.rescale_ts(tb, octx.stream(0).unwrap().time_base());
            pkt.write_interleaved(octx).unwrap();
        }
    };
    let mut frame = ff::frame::Video::new(ff::format::Pixel::YUV420P10LE, W, H);
    for i in 0..FRAMES {
        let rgba = render_tagged_frame(
            FrameTag {
                clip: 7,
                frame: i as u16,
            },
            W,
            H,
        );
        let yuv = rgba_to_yuv420p(&rgba, W, H);
        let (cw, ch) = yuv.chroma_size();
        for (plane, src, pw, ph) in [
            (0, &yuv.y, W as usize, H as usize),
            (1, &yuv.u, cw, ch),
            (2, &yuv.v, cw, ch),
        ] {
            let stride = frame.stride(plane);
            let data = frame.data_mut(plane);
            for y in 0..ph {
                for x in 0..pw {
                    let v = u16::from(src[y * pw + x]) << 2;
                    let o = y * stride + x * 2;
                    data[o..o + 2].copy_from_slice(&v.to_le_bytes());
                }
            }
        }
        frame.set_pts(Some(i as i64));
        enc.send_frame(&frame).unwrap();
        drain(&mut enc, &mut octx);
    }
    enc.send_eof().unwrap();
    drain(&mut enc, &mut octx);
    octx.write_trailer().unwrap();
    Some(FRAMES)
}

#[test]
fn ten_bit_hevc_decodes_to_rgba8() {
    let path = out_dir().join("hevc10-tagged.mkv");
    let Some(frames) = write_hevc10(&path) else {
        eprintln!("SKIPPED: libx265 is not available in this FFmpeg");
        return;
    };
    let b = backend();
    let mut r = b.open_video(&path, &DecodeOptions::default()).unwrap();
    assert_eq!(r.info().codec, "hevc");
    assert_eq!(r.info().bit_depth, 10);
    let mut n = 0u16;
    while let Some(f) = r.next_frame().unwrap() {
        assert_eq!((f.width, f.height, f.data.len()), (320, 240, 320 * 240 * 4));
        assert_eq!(tag_of(&f), Some(FrameTag { clip: 7, frame: n }));
        n += 1;
    }
    assert_eq!(n as usize, frames);

    let mut r = b
        .open_video(
            &path,
            &DecodeOptions {
                size: Some((640, 480)),
                apply_rotation: false,
            },
        )
        .unwrap();
    let f = r.frame_at(Micros(500_000)).unwrap().unwrap();
    assert_eq!((f.width, f.height), (640, 480));
    assert_eq!(tag_of(&f), Some(FrameTag { clip: 7, frame: 12 }));
}
