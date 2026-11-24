//! Filmstrip thumbnails on tagged fixture clips from `kv-testkit`: every
//! frame shows its frame index, so each thumbnail can be identified.
#![cfg(feature = "ffmpeg")]
// The rotation fixture sets the display matrix, which `ffmpeg-next` has no
// setter for; the `unsafe` block writes through pointers owned by a live
// `ffmpeg-next` wrapper.
#![allow(unsafe_code)]

use std::path::{Path, PathBuf};
use std::time::Instant;

use ff::ffi;
use ffmpeg_next as ff;
use kv_core::Micros;
use kv_media::{
    encode_jpeg, filmstrip, filmstrip_size, DecodeOptions, FfmpegBackend, MediaBackend, MediaError,
    VideoFrame, FILMSTRIP_KEYFRAME_SNAP,
};
use kv_testkit::{read_frame_tag, tagged_clip, ClipSpec};

fn backend() -> FfmpegBackend {
    FfmpegBackend::new().expect("FFmpeg initialises")
}

fn out_dir() -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("kv-media-filmstrip");
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn tag_frame(f: &VideoFrame) -> u16 {
    read_frame_tag(&f.data, f.width, f.height)
        .unwrap_or_else(|| panic!("thumbnail at {:?} has no readable tag", f.pts))
        .frame
}

/// Cell centre `duration * (i + 0.5) / count`, as the filmstrip computes it.
fn centre(duration: Micros, i: u32, count: u32) -> Micros {
    Micros(duration.0 * i64::from(2 * i + 1) / i64::from(2 * count))
}

/// Index of the frame displayed at `t` in a clip at `fps`.
fn frame_index(t: Micros, fps: (i32, i32)) -> i64 {
    t.0 * i64::from(fps.0) / (1_000_000 * i64::from(fps.1))
}

#[test]
fn eight_thumbnails_are_the_cell_centres_in_order() {
    let spec = ClipSpec {
        clip: 5,
        ..ClipSpec::default()
    };
    let path = tagged_clip(&spec);
    let duration = backend().probe(&path).unwrap().duration;
    let mut r = backend()
        .open_video_reader(&path, &DecodeOptions::default())
        .unwrap();

    let thumbs = filmstrip(&mut r, duration, 8, 120).unwrap();
    assert_eq!(thumbs.len(), 8);
    let mut prev = None;
    for (i, t) in thumbs.iter().enumerate() {
        assert_eq!((t.width, t.height, t.data.len()), (160, 120, 160 * 120 * 4));
        let got = i64::from(tag_frame(t));
        let want = frame_index(centre(duration, i as u32, 8), spec.fps);
        assert!(
            (got - want).abs() <= 1,
            "thumbnail {i}: frame {got}, expected {want} +-1"
        );
        assert!(prev.is_none_or(|p| got > p), "tags strictly increase");
        prev = Some(got);
    }
    assert!(r.seek_count() <= 8, "{} seeks", r.seek_count());
}

#[test]
fn reader_opened_at_thumbnail_size_needs_no_rescale() {
    let spec = ClipSpec {
        clip: 6,
        ..ClipSpec::default()
    };
    let path = tagged_clip(&spec);
    let duration = backend().probe(&path).unwrap().duration;
    let info = backend().probe(&path).unwrap().video.unwrap();
    let size = filmstrip_size(&info, 90).unwrap();
    assert_eq!(size, (120, 90));
    let opts = DecodeOptions {
        size: Some(size),
        apply_rotation: true,
    };
    let mut r = backend().open_video(&path, &opts).unwrap();
    let thumbs = filmstrip(r.as_mut(), duration, 4, 90).unwrap();
    for (i, t) in thumbs.iter().enumerate() {
        assert_eq!((t.width, t.height), size);
        let want = frame_index(centre(duration, i as u32, 4), spec.fps);
        assert!((i64::from(tag_frame(t)) - want).abs() <= 1);
    }
}

#[test]
fn far_apart_cells_snap_to_a_nearby_keyframe() {
    // 12 s with a keyframe every second (kv-testkit's GOP is 1 s). Four
    // cells 3 s apart: reaching each one after the first needs a seek, and
    // a keyframe at most 0.5 s before the centre is used as is.
    let spec = ClipSpec {
        clip: 7,
        seconds: 12.0,
        ..ClipSpec::default()
    };
    let path = tagged_clip(&spec);
    let duration = backend().probe(&path).unwrap().duration;
    let mut r = backend()
        .open_video_reader(&path, &DecodeOptions::default())
        .unwrap();
    let thumbs = filmstrip(&mut r, duration, 4, 120).unwrap();

    let snap_frames = frame_index(FILMSTRIP_KEYFRAME_SNAP, spec.fps);
    let mut prev = -1;
    let mut snapped = 0;
    for (i, t) in thumbs.iter().enumerate() {
        let c = centre(duration, i as u32, 4);
        let got = i64::from(tag_frame(t));
        let exact = frame_index(c, spec.fps);
        assert!(
            got <= exact + 1 && got >= exact - snap_frames - 1,
            "thumbnail {i}: frame {got}, exact {exact}"
        );
        assert!(t.pts <= c && c.0 - t.pts.0 <= FILMSTRIP_KEYFRAME_SNAP.0 + 1);
        assert!(got > prev, "tags strictly increase");
        prev = got;
        if got < exact - 1 {
            snapped += 1;
        }
    }
    assert!(snapped > 0, "no thumbnail used keyframe-only seeking");
    assert!(r.seek_count() <= 4, "{} seeks", r.seek_count());
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

#[test]
fn rotated_clip_gives_portrait_thumbnails() {
    let src = tagged_clip(&ClipSpec {
        clip: 8,
        ..ClipSpec::default()
    });
    let path = out_dir().join("rotated-90.mp4");
    remux_with_rotation(&src, &path, 90.0);
    let duration = backend().probe(&path).unwrap().duration;
    let mut r = backend()
        .open_video(&path, &DecodeOptions::default())
        .unwrap();
    assert_eq!(r.info().rotation, 90);
    let thumbs = filmstrip(r.as_mut(), duration, 3, 64).unwrap();
    for t in &thumbs {
        // 320x240 shown upright is 240x320: 48x64 at 64 px high.
        assert_eq!((t.width, t.height), (48, 64));
    }
}

#[test]
fn out_of_range_arguments_are_errors() {
    let path = tagged_clip(&ClipSpec::default());
    let duration = backend().probe(&path).unwrap().duration;
    let mut r = backend()
        .open_video(&path, &DecodeOptions::default())
        .unwrap();
    for (count, height) in [(0, 64), (65, 64), (8, 15), (8, 361)] {
        assert!(
            matches!(
                filmstrip(r.as_mut(), duration, count, height),
                Err(MediaError::InvalidThumbnail(_))
            ),
            "{count} x {height}"
        );
    }
}

#[test]
fn jpeg_of_a_thumbnail_decodes_at_the_right_size() {
    let spec = ClipSpec {
        clip: 9,
        ..ClipSpec::default()
    };
    let path = tagged_clip(&spec);
    let duration = backend().probe(&path).unwrap().duration;
    let mut r = backend()
        .open_video(&path, &DecodeOptions::default())
        .unwrap();
    let thumbs = filmstrip(r.as_mut(), duration, 2, 96).unwrap();
    for t in &thumbs {
        let jpeg = encode_jpeg(t, 80).unwrap();
        let file = out_dir().join(format!("thumb-{}.jpg", t.pts.0));
        std::fs::write(&file, &jpeg).unwrap();
        // Decode with FFmpeg (an independent JPEG decoder).
        let mut ictx = ff::format::input(&file).unwrap();
        let stream = ictx.streams().best(ff::media::Type::Video).unwrap();
        let idx = stream.index();
        let mut dec = ff::codec::Context::from_parameters(stream.parameters())
            .unwrap()
            .decoder()
            .video()
            .unwrap();
        let mut frame = ff::frame::Video::empty();
        let mut decoded = false;
        for (s, packet) in ictx.packets() {
            if s.index() == idx {
                dec.send_packet(&packet).unwrap();
                if dec.receive_frame(&mut frame).is_ok() {
                    decoded = true;
                    break;
                }
            }
        }
        if !decoded {
            dec.send_eof().unwrap();
            dec.receive_frame(&mut frame).unwrap();
        }
        assert_eq!((frame.width(), frame.height()), (128, 96));
        assert_eq!((t.width, t.height), (128, 96));
    }
}

/// Issue #49: 12 thumbnails of a 1080p 60 s clip should take under 1 s in
/// release. Logged, not asserted (runner speed varies). Encoding the 60 s
/// 1080p fixture is slow, so this runs in the full regression.
#[test]
#[ignore = "regression"]
fn twelve_thumbnails_of_1080p_60s_timing() {
    let path = tagged_clip(&ClipSpec {
        clip: 10,
        seconds: 60.0,
        width: 1920,
        height: 1080,
        ..ClipSpec::default()
    });
    let probe = backend().probe(&path).unwrap();
    let size = filmstrip_size(probe.video.as_ref().unwrap(), 90).unwrap();
    let start = Instant::now();
    let opts = DecodeOptions {
        size: Some(size),
        apply_rotation: true,
    };
    let mut r = backend().open_video(&path, &opts).unwrap();
    let thumbs = filmstrip(r.as_mut(), probe.duration, 12, 90).unwrap();
    let elapsed = start.elapsed();
    assert_eq!(thumbs.len(), 12);
    println!(
        "filmstrip: 12 thumbnails ({}x{}) of a 1080p 60 s clip in {elapsed:?} (target < 1 s in release)",
        size.0, size.1
    );
}
