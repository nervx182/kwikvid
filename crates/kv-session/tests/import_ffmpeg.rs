//! `Session::import` and filmstrips end to end with FFmpeg on tagged
//! `kv-testkit` clips: two single recordings and one chaptered recording.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use kv_audio::MusicAnalysis;
use kv_cache::Cache;
use kv_core::Micros;
use kv_media::FfmpegBackend;
use kv_session::{
    JobContext, JobEvent, JobRunner, MusicAnalyzer, Session, FILMSTRIP_COUNT, FILMSTRIP_HEIGHT,
};
use kv_testkit::{clip_colour, tagged_clip_set};

struct NoMusic;

impl MusicAnalyzer for NoMusic {
    fn analyze(&self, _: &Path) -> Result<MusicAnalysis, Box<dyn std::error::Error + Send + Sync>> {
        Err("no music in this test".into())
    }
}

/// RGB distance of the thumbnail's top-left background pixel to `rgb`.
fn corner_distance(jpeg: &[u8], rgb: [u8; 3]) -> f64 {
    let img = image::load_from_memory_with_format(jpeg, image::ImageFormat::Jpeg)
        .unwrap()
        .to_rgb8();
    let p = img.get_pixel(2, 2).0;
    p.iter()
        .zip(rgb)
        .map(|(&a, b)| (f64::from(a) - f64::from(b)).powi(2))
        .sum::<f64>()
        .sqrt()
}

#[test]
fn imports_real_clips_and_makes_filmstrips() {
    let clips = tagged_clip_set(2, 1.0);
    let dir = tempfile::tempdir().unwrap();
    let singles = dir.path().join("singles");
    let chaptered = dir.path().join("chaptered");
    std::fs::create_dir_all(&singles).unwrap();
    std::fs::create_dir_all(&chaptered).unwrap();
    for clip in &clips {
        std::fs::copy(clip, singles.join(clip.file_name().unwrap())).unwrap();
    }
    std::fs::copy(&clips[0], chaptered.join("GX010005.MP4")).unwrap();
    std::fs::copy(&clips[1], chaptered.join("GX020005.MP4")).unwrap();

    let media = Arc::new(FfmpegBackend::new().unwrap());
    let ctx = JobContext::new(
        media,
        Arc::new(NoMusic),
        Arc::new(Cache::in_memory().unwrap()),
    );
    let mut session = Session::new(dir.path().to_path_buf());
    let (report, snapshot) = session.import(&[singles, chaptered], &ctx).unwrap();
    assert!(report.skipped.is_empty(), "{:?}", report.skipped);
    assert_eq!(report.added.len(), 3);

    let media = &snapshot.snapshot.project.media;
    let paths: Vec<_> = media.iter().map(|m| m.path.as_str()).collect();
    // Grouped and ordered by folder (see `kv_gopro::group_chapters`).
    assert_eq!(
        paths,
        [
            "chaptered/GX010005.MP4",
            "singles/GX010001.MP4",
            "singles/GX010002.MP4"
        ]
    );
    let near = |d: Micros, secs: f64| (d.0 as f64 / 1e6 - secs).abs() < 0.1;
    assert_eq!(media[0].chapters.len(), 2);
    assert!(near(media[0].duration, 2.0), "{:?}", media[0].duration);
    assert!(near(media[1].duration, 1.0), "{:?}", media[1].duration);

    let (runner, events) = JobRunner::new(ctx);
    for &id in &report.added {
        runner.submit(session.filmstrip_job(id).unwrap());
    }
    let mut finished = 0;
    while finished < report.added.len() {
        let msg = events.recv_timeout(Duration::from_secs(120)).unwrap();
        session.handle_job_message(&msg);
        if msg.event.is_final() {
            assert!(matches!(msg.event, JobEvent::Done { .. }), "{msg:?}");
            finished += 1;
        }
    }

    for &id in &report.added {
        for i in 0..FILMSTRIP_COUNT as usize {
            let jpeg = session.thumbnail(id, i).expect("thumbnail");
            let img = image::load_from_memory(&jpeg).unwrap();
            assert_eq!((img.width(), img.height()), (120, FILMSTRIP_HEIGHT));
        }
    }
    // The chaptered item's filmstrip runs across both chapters: clip 0's
    // colour first, clip 1's in the second half.
    let chaptered_id = report.added[0];
    let first = session.thumbnail(chaptered_id, 0).unwrap();
    let last = session.thumbnail(chaptered_id, 11).unwrap();
    assert!(corner_distance(&first, clip_colour(0)) < 40.0);
    assert!(corner_distance(&last, clip_colour(1)) < 40.0);
}
