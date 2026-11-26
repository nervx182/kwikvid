//! `ChapteredReader` over real files: two tagged clips (#24) stand in for
//! the chapter files of one recording, so every frame across the boundary
//! can be identified. Real chaptered GoPro footage belongs to the
//! regression suite.
#![cfg(feature = "ffmpeg")]

use kv_core::Micros;
use kv_media::{
    open_item_video, Chapter, ChapteredReader, DecodeOptions, FfmpegBackend, MediaBackend,
    VideoFrame, VideoReader,
};
use kv_testkit::{read_frame_tag, tagged_clip, ClipSpec, FrameTag};

fn tag_of(f: &VideoFrame) -> FrameTag {
    read_frame_tag(&f.data, f.width, f.height).expect("a tagged frame")
}

/// Chapter 1 (clip 1, 1 s) then chapter 2 (clip 2, 1.5 s), 30 fps, with
/// their probed durations as an import would record them.
fn chapters(backend: &FfmpegBackend) -> Vec<Chapter> {
    [(1, 1.0), (2, 1.5)]
        .into_iter()
        .map(|(clip, seconds)| {
            let path = tagged_clip(&ClipSpec {
                clip,
                seconds,
                ..ClipSpec::default()
            });
            let duration = backend.probe(&path).unwrap().duration;
            Chapter { path, duration }
        })
        .collect()
}

/// Every (tag, pts) of the item, in order, as decoded by `next_frame`.
fn decode_all(r: &mut dyn VideoReader) -> Vec<(FrameTag, Micros)> {
    std::iter::from_fn(|| r.next_frame().unwrap())
        .map(|f| (tag_of(&f), f.pts))
        .collect()
}

#[test]
fn next_frame_runs_across_the_chapter_files() {
    let backend = FfmpegBackend::new().unwrap();
    let chapters = chapters(&backend);
    let offset = chapters[0].duration;
    let mut r = ChapteredReader::open(&backend, chapters, &DecodeOptions::default()).unwrap();
    let frames = decode_all(&mut r);

    let want: Vec<FrameTag> = (0..30u16)
        .map(|frame| FrameTag { clip: 1, frame })
        .chain((0..45u16).map(|frame| FrameTag { clip: 2, frame }))
        .collect();
    let tags: Vec<FrameTag> = frames.iter().map(|f| f.0).collect();
    assert_eq!(tags, want, "every frame once, in order");

    for w in frames.windows(2) {
        assert!(w[0].1 < w[1].1, "pts increase: {:?} then {:?}", w[0], w[1]);
    }
    // Chapter 2's frames are offset by chapter 1's duration.
    let frame_us = |i: i64| (i * 1_000_000 + 15) / 30;
    for (k, (tag, pts)) in frames.iter().enumerate().skip(30) {
        let want = offset.0 + frame_us(i64::from(tag.frame));
        assert!(
            (pts.0 - want).abs() <= 1_000,
            "frame {k}: {pts:?} vs {want}"
        );
    }
    // The boundary has no gap: one frame interval, give or take rounding.
    let step = frames[30].1 .0 - frames[29].1 .0;
    assert!((step - frame_us(1)).abs() <= 1_000, "step {step} us");
}

#[test]
fn frame_at_and_seek_across_the_boundary() {
    let backend = FfmpegBackend::new().unwrap();
    let chapters = chapters(&backend);
    let frames = {
        let mut r =
            ChapteredReader::open(&backend, chapters.clone(), &DecodeOptions::default()).unwrap();
        decode_all(&mut r)
    };
    let mut r = open_item_video(
        &backend,
        &chapters[0].path,
        &chapters,
        &DecodeOptions::default(),
    )
    .unwrap();

    // Every frame around the boundary, forwards, at its pts and just before.
    for k in 20..40 {
        let (tag, pts) = frames[k];
        assert_eq!(tag_of(&r.frame_at(pts).unwrap().unwrap()), tag, "frame {k}");
        let before = r.frame_at(Micros(pts.0 - 1)).unwrap().unwrap();
        assert_eq!(tag_of(&before), frames[k - 1].0, "just before frame {k}");
    }
    // Backwards over the boundary.
    for k in (25..35).rev() {
        let (tag, pts) = frames[k];
        assert_eq!(tag_of(&r.frame_at(pts).unwrap().unwrap()), tag, "back {k}");
    }

    // Seek into the later chapter, then decode on.
    r.seek(frames[50].1).unwrap();
    let rest = decode_all(r.as_mut());
    assert_eq!(rest, frames[50..]);
    // Seek back into the first chapter: decoding continues into the second.
    r.seek(frames[28].1).unwrap();
    let next: Vec<_> = (0..4)
        .map(|_| tag_of(&r.next_frame().unwrap().unwrap()))
        .collect();
    let want: Vec<_> = frames[28..32].iter().map(|f| f.0).collect();
    assert_eq!(next, want);
}

#[test]
fn an_item_without_chapters_reads_its_single_file() {
    let backend = FfmpegBackend::new().unwrap();
    let chapters = chapters(&backend);
    let mut r =
        open_item_video(&backend, &chapters[1].path, &[], &DecodeOptions::default()).unwrap();
    let frames = decode_all(r.as_mut());
    assert_eq!(frames.len(), 45);
    assert_eq!(frames[0].0, FrameTag { clip: 2, frame: 0 });
    assert!(frames[0].1 .0 < 1_000, "not offset");
}
