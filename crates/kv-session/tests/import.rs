//! `Session::import` and library thumbnails, with a fake media backend and a
//! fake filmstrip generator (no FFmpeg).

use std::error::Error;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use kv_audio::MusicAnalysis;
use kv_cache::Cache;
use kv_core::{MediaId, MediaKind, Micros};
use kv_media::{DecodeOptions, MediaBackend, MediaError, MediaInfo, Rational, VideoInfo};
use kv_session::{
    FilmstripGenerator, Job, JobContext, JobEvent, JobMessage, JobResult, JobRunner, MusicAnalyzer,
    Session, FILMSTRIP_COUNT, FILMSTRIP_HEIGHT,
};

const SECOND: i64 = 1_000_000;

/// Probes by file name: `*bad*` fails, `*audio*` has no video stream,
/// anything else is a 2 s video.
#[derive(Default)]
struct FakeMedia {
    probes: AtomicUsize,
}

impl MediaBackend for FakeMedia {
    fn probe(&self, path: &Path) -> kv_media::Result<MediaInfo> {
        self.probes.fetch_add(1, Ordering::SeqCst);
        let name = path.file_name().unwrap().to_string_lossy().to_lowercase();
        if name.contains("bad") {
            return Err(MediaError::NoStreams(path.to_path_buf()));
        }
        let video = (!name.contains("audio")).then(|| VideoInfo {
            codec: "fake".into(),
            width: 320,
            height: 240,
            fps: Rational::new(30, 1),
            bit_depth: 8,
            rotation: 0,
        });
        Ok(MediaInfo {
            duration: Micros(2 * SECOND),
            container: "fake".into(),
            video,
            audio: None,
            creation_time: None,
        })
    }

    fn open_video(
        &self,
        path: &Path,
        _: &DecodeOptions,
    ) -> kv_media::Result<Box<dyn kv_media::VideoReader>> {
        Err(MediaError::NoVideoStream(path.to_path_buf()))
    }
}

struct NoMusic;

impl MusicAnalyzer for NoMusic {
    fn analyze(&self, _: &Path) -> Result<MusicAnalysis, Box<dyn Error + Send + Sync>> {
        Err("no music in this test".into())
    }
}

/// Thumbnail `i` is the bytes `[i, files.len()]`; records each call.
#[derive(Default)]
struct FakeFilmstrips {
    calls: Mutex<Vec<Vec<(PathBuf, Micros)>>>,
}

impl FilmstripGenerator for FakeFilmstrips {
    fn generate(
        &self,
        files: &[(PathBuf, Micros)],
        count: u32,
        height: u32,
    ) -> Result<Vec<Vec<u8>>, Box<dyn Error + Send + Sync>> {
        assert_eq!((count, height), (FILMSTRIP_COUNT, FILMSTRIP_HEIGHT));
        self.calls.lock().unwrap().push(files.to_vec());
        Ok((0..count)
            .map(|i| vec![i as u8, files.len() as u8])
            .collect())
    }
}

struct Fixture {
    dir: tempfile::TempDir,
    media: Arc<FakeMedia>,
    filmstrips: Arc<FakeFilmstrips>,
    ctx: JobContext,
    session: Session,
}

impl Fixture {
    fn new() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let media = Arc::new(FakeMedia::default());
        let filmstrips = Arc::new(FakeFilmstrips::default());
        let ctx = JobContext {
            media: media.clone(),
            analyzer: Arc::new(NoMusic),
            cache: Arc::new(Cache::in_memory().unwrap()),
            filmstrips: filmstrips.clone(),
        };
        let session = Session::new(dir.path().to_path_buf());
        Fixture {
            dir,
            media,
            filmstrips,
            ctx,
            session,
        }
    }

    /// Create `rel` (with parent folders) holding a few bytes.
    fn file(&self, rel: &str) -> PathBuf {
        let path = self.dir.path().join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, rel.as_bytes()).unwrap();
        path
    }

    fn import(&mut self, paths: &[PathBuf]) -> kv_session::ImportReport {
        self.session.import(paths, &self.ctx).unwrap().0
    }

    fn reasons(report: &kv_session::ImportReport) -> Vec<(String, String)> {
        report
            .skipped
            .iter()
            .map(|(p, r)| {
                (
                    p.file_name().unwrap().to_string_lossy().into_owned(),
                    r.clone(),
                )
            })
            .collect()
    }
}

#[test]
fn folder_scan_is_flat_case_insensitive_and_skips_sidecars() {
    let mut fx = Fixture::new();
    fx.file("card/a.MP4");
    fx.file("card/b.mov");
    fx.file("card/GX010001.LRV");
    fx.file("card/GX010001.THM");
    fx.file("card/notes.txt");
    fx.file("card/sub/deep.mp4");
    let card = fx.dir.path().join("card");
    let report = fx.import(&[card]);

    assert_eq!(report.added, vec![MediaId(1), MediaId(2)]);
    assert!(report.skipped.is_empty(), "{:?}", report.skipped);
    let paths: Vec<_> = fx
        .session
        .project()
        .media
        .iter()
        .map(|m| m.path.as_str())
        .collect();
    assert_eq!(paths, ["card/a.MP4", "card/b.mov"]);
    let a = &fx.session.project().media[0];
    assert_eq!(a.duration, Micros(2 * SECOND));
    assert_eq!(a.kind, MediaKind::Video);
    assert!(a.hilights.is_empty(), "not a GoPro MP4: no HiLights");
}

#[test]
fn gopro_chapters_become_one_item() {
    let mut fx = Fixture::new();
    // Given out of order: chapter 2 first.
    let files = [
        fx.file("GX020007.MP4"),
        fx.file("GX010007.MP4"),
        fx.file("GX010008.MP4"),
    ];
    let report = fx.import(&files);

    assert_eq!(report.added.len(), 2);
    let media = &fx.session.project().media;
    assert_eq!(media[0].path, "GX010007.MP4");
    let chapters: Vec<_> = media[0].chapters.iter().map(|c| c.path.as_str()).collect();
    assert_eq!(chapters, ["GX010007.MP4", "GX020007.MP4"]);
    assert_eq!(media[0].duration, Micros(4 * SECOND));
    assert_eq!(media[1].path, "GX010008.MP4");
    assert!(media[1].chapters.is_empty());
}

#[test]
fn unsupported_files_are_reported_not_added() {
    let mut fx = Fixture::new();
    let paths = [
        fx.file("good.mp4"),
        fx.file("bad.mp4"),
        fx.file("audio.mov"),
        fx.file("photo.jpg"),
        fx.dir.path().join("missing.mp4"),
    ];
    let report = fx.import(&paths);

    assert_eq!(report.added, vec![MediaId(1)]);
    let reasons = Fixture::reasons(&report);
    let names: Vec<_> = reasons.iter().map(|(n, _)| n.as_str()).collect();
    // Extension check first, then probing in name order.
    assert_eq!(names, ["photo.jpg", "audio.mov", "bad.mp4", "missing.mp4"]);
    assert!(reasons[0].1.contains("MP4 and MOV"), "{reasons:?}");
    assert_eq!(reasons[1].1, "It has no video.");
    assert!(reasons[2].1.contains("bad.mp4"), "{reasons:?}");
    assert!(reasons[3].1.contains("missing.mp4"), "{reasons:?}");
    assert_eq!(fx.session.project().media.len(), 1);
}

#[test]
fn reimporting_skips_files_already_in_the_library() {
    let mut fx = Fixture::new();
    let a = fx.file("a.mp4");
    let ch1 = fx.file("GX010003.MP4");
    let ch2 = fx.file("GX020003.MP4");
    fx.import(&[a.clone(), ch1.clone(), ch2.clone()]);
    let probes = fx.media.probes.load(Ordering::SeqCst);

    // The same file twice in one call, the same files again (one through a
    // `..` path), and a new one.
    let b = fx.file("b.mp4");
    let a_again = fx.dir.path().join("x/../a.mp4");
    let report = fx.import(&[a_again, ch2, b.clone(), b]);

    assert_eq!(report.added, vec![MediaId(3)]);
    let reasons = Fixture::reasons(&report);
    assert_eq!(
        reasons,
        [
            ("a.mp4".into(), "Already in the library.".into()),
            ("GX020003.MP4".into(), "Already in the library.".into()),
            ("b.mp4".into(), "Already in the library.".into()),
        ]
    );
    assert_eq!(fx.media.probes.load(Ordering::SeqCst), probes + 1);
}

#[test]
fn whole_import_is_one_undo_step_and_empty_import_records_none() {
    let mut fx = Fixture::new();
    let paths = [fx.file("a.mp4"), fx.file("b.mp4"), fx.file("c.mp4")];
    let (report, snapshot) = fx.session.import(&paths, &fx.ctx).unwrap();
    assert_eq!(report.added.len(), 3);
    assert!(snapshot.can_undo && snapshot.dirty);

    let (report, again) = fx.session.import(&paths, &fx.ctx).unwrap();
    assert!(report.added.is_empty());
    assert_eq!(again.revision, snapshot.revision, "nothing changed");

    let undone = fx.session.undo().unwrap();
    assert!(undone.project.media.is_empty());
    assert!(!undone.can_undo);
}

#[test]
fn probe_results_come_from_the_cache_on_reimport_after_undo() {
    let mut fx = Fixture::new();
    let a = fx.file("a.mp4");
    fx.import(std::slice::from_ref(&a));
    fx.session.undo().unwrap();
    fx.import(&[a]);
    assert_eq!(fx.media.probes.load(Ordering::SeqCst), 1);
}

fn wait_done(events: &Receiver<JobMessage>, session: &mut Session) -> JobMessage {
    loop {
        let msg = events.recv_timeout(Duration::from_secs(20)).unwrap();
        session.handle_job_message(&msg);
        if msg.event.is_final() {
            return msg;
        }
    }
}

#[test]
fn filmstrip_jobs_store_thumbnails_for_the_session_and_the_cache() {
    let mut fx = Fixture::new();
    let ch1 = fx.file("GX010009.MP4");
    let ch2 = fx.file("GX020009.MP4");
    let single = fx.file("single.mov");
    let report = fx.import(&[ch1.clone(), ch2.clone(), single]);
    let (chaptered, single_id) = (report.added[0], report.added[1]);
    assert_eq!(fx.session.thumbnail(chaptered, 0), None);

    let job = fx.session.filmstrip_job(chaptered).unwrap();
    assert_eq!(
        job,
        Job::Filmstrip {
            media_id: chaptered,
            files: vec![(ch1, Micros(2 * SECOND)), (ch2, Micros(2 * SECOND))],
        }
    );

    let (runner, events) = JobRunner::new(fx.ctx.clone());
    runner.submit(job.clone());
    let done = wait_done(&events, &mut fx.session);
    assert!(matches!(
        done.event,
        JobEvent::Done {
            result: JobResult::Filmstrip(ref t)
        } if t.len() == FILMSTRIP_COUNT as usize
    ));
    assert_eq!(fx.session.thumbnail(chaptered, 0), Some(vec![0, 2]));
    assert_eq!(fx.session.thumbnail(chaptered, 11), Some(vec![11, 2]));
    assert_eq!(fx.session.thumbnail(chaptered, 12), None);
    assert_eq!(fx.session.thumbnail(single_id, 0), None);
    assert_eq!(fx.session.thumbnail(MediaId(99), 0), None);

    // A second run (say, after reopening the project) hits the cache.
    runner.submit(job);
    wait_done(&events, &mut fx.session);
    assert_eq!(fx.filmstrips.calls.lock().unwrap().len(), 1);

    runner.submit(fx.session.filmstrip_job(single_id).unwrap());
    wait_done(&events, &mut fx.session);
    assert_eq!(fx.session.thumbnail(single_id, 3), Some(vec![3, 1]));
    assert_eq!(fx.filmstrips.calls.lock().unwrap().len(), 2);
}

#[test]
fn thumbnails_follow_the_project_through_save_as() {
    let mut fx = Fixture::new();
    let a = fx.file("clips/a.mp4");
    let id = fx.import(&[a]).added[0];
    let (runner, events) = JobRunner::new(fx.ctx.clone());
    runner.submit(fx.session.filmstrip_job(id).unwrap());
    wait_done(&events, &mut fx.session);

    let project = fx.dir.path().join("projects/trip.kwikvid");
    std::fs::create_dir_all(project.parent().unwrap()).unwrap();
    let snap = fx.session.save_as(&project).unwrap();
    assert_eq!(snap.project.media[0].path, "../clips/a.mp4");
    assert_eq!(fx.session.thumbnail(id, 0), Some(vec![0, 1]));
}

#[test]
fn no_filmstrip_job_for_unknown_items() {
    let fx = Fixture::new();
    assert_eq!(fx.session.filmstrip_job(MediaId(1)), None);
}
