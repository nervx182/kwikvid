//! The export job end to end: two tagged clips through the real FFmpeg
//! backend at a tiny 9:16 size, and a cancel part-way.

use std::path::Path;
use std::sync::mpsc;
use std::sync::Arc;
use std::time::Duration;

use kv_core::{
    AspectRatio, Command, Edl, MediaId, MediaItem, MediaKind, Micros, Project, Segment, Speed,
    Transition,
};
use kv_media::{ExportSettings, FfmpegBackend, MediaBackend, VideoCodec};
use kv_session::{ExportEvent, ExportJob, ExportRequest, ExportResolution, Session};
use kv_testkit::{tagged_clip, ClipSpec};

const SEC: i64 = 1_000_000;
const TIMEOUT: Duration = Duration::from_secs(60);
/// 9:16 at a test-friendly size (an iPhone-ish 320x568).
const SIZE: (u32, u32) = (320, 568);

/// A session with two tagged 2 s clips and an edit of `secs_each` of each.
fn session(root: &Path, secs_each: f64) -> Session {
    let mut s = Session::new(root.to_path_buf());
    for (id, clip) in [(1, 0u8), (2, 6)] {
        let path = tagged_clip(&ClipSpec {
            clip,
            ..ClipSpec::default()
        });
        let item = MediaItem {
            id: MediaId(id),
            path: s.to_project_path(&path).unwrap(),
            duration: Micros(2 * SEC),
            kind: MediaKind::Video,
            hilights: vec![],
            clip_audio: false,
            chapters: vec![],
        };
        s.apply(Command::AddMedia { item }).unwrap();
    }
    let len = Micros((secs_each * 1e6) as i64);
    let mut edl = Edl {
        segments: [1, 2]
            .map(|id| Segment {
                media_id: MediaId(id),
                src_in: Micros(0),
                src_out: len,
                timeline_start: Micros(0),
                transition: Transition::Cut,
                speed: Speed::NORMAL,
            })
            .to_vec(),
    };
    edl.repack();
    s.apply(Command::ReplaceEdl {
        section: Project::DEFAULT_SECTION,
        edl,
    })
    .unwrap();
    s.apply(Command::SetAspectRatio {
        aspect: AspectRatio::Portrait9x16,
    })
    .unwrap();
    s
}

/// The input of a tiny 9:16 export of `s` to `out`.
fn input(s: &Session, out: &Path) -> kv_session::ExportInput {
    let request = ExportRequest {
        codec: VideoCodec::H264,
        resolution: ExportResolution::Hd1080,
        aspect: s.project().settings.aspect_ratio,
        path: out.to_path_buf(),
    };
    let mut input = s
        .export_input(Project::DEFAULT_SECTION, &request)
        .expect("exportable");
    assert_eq!((input.settings.width, input.settings.height), (1080, 1920));
    input.settings.width = SIZE.0;
    input.settings.height = SIZE.1;
    input.settings.video_bitrate =
        ExportSettings::default_bitrate(SIZE.0, SIZE.1, VideoCodec::H264);
    input
}

fn backend() -> Arc<FfmpegBackend> {
    Arc::new(FfmpegBackend::new().unwrap())
}

#[test]
fn export_job_writes_a_portrait_file_of_both_clips() {
    let dir = tempfile::tempdir().unwrap();
    // Canonical, so the relative clip paths survive macOS's /var -> /private/var link.
    let root = dir.path().canonicalize().unwrap();
    let s = session(&root, 1.0);
    let out = root.join("Trip.mp4");
    let ff = backend();
    let (tx, rx) = mpsc::channel();
    let job = ExportJob::start(input(&s, &out), ff.clone(), ff.clone(), move |e| {
        let _ = tx.send(e);
    });
    let mut events = Vec::new();
    loop {
        let e = rx.recv_timeout(TIMEOUT).expect("export event");
        let last = e.is_final();
        events.push(e);
        if last {
            break;
        }
    }
    job.wait();

    assert_eq!(events[0], ExportEvent::Started);
    let fractions: Vec<f32> = events
        .iter()
        .filter_map(|e| match e {
            ExportEvent::Progress { fraction, .. } => Some(*fraction),
            _ => None,
        })
        .collect();
    assert!(fractions.windows(2).all(|w| w[0] < w[1]), "{fractions:?}");
    assert_eq!(fractions.last(), Some(&1.0));
    let ExportEvent::Done { report } = events.last().unwrap() else {
        panic!("expected Done, got {:?}", events.last());
    };
    assert_eq!(report.path, out);
    assert_eq!(report.frames, 60);
    assert_eq!(report.duration, Micros(2 * SEC));
    assert!(!report.encoder_name.is_empty());

    let info = ff.probe(&out).unwrap();
    let video = info.video.expect("video stream");
    assert_eq!((video.width, video.height), SIZE);
    assert!(!kv_session::export::partial_path(&out).exists());
    assert_eq!(info.duration.0 / 100_000, 20, "2 s: {:?}", info.duration);
}

#[test]
fn cancel_mid_way_leaves_no_file() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let s = session(&root, 2.0);
    let out = root.join("Cancelled.mp4");
    let ff = backend();
    let (tx, rx) = mpsc::channel();
    let (ack_tx, ack_rx) = mpsc::channel::<()>();
    // At 10% the export thread waits until the test has cancelled, so the
    // cancel lands mid-way deterministically.
    let job = ExportJob::start(input(&s, &out), ff.clone(), ff, move |e| {
        let pause = matches!(e, ExportEvent::Progress { frame: 12, .. });
        let _ = tx.send(e);
        if pause {
            let _ = ack_rx.recv_timeout(TIMEOUT);
        }
    });
    let last = loop {
        let e = rx.recv_timeout(TIMEOUT).expect("export event");
        if matches!(e, ExportEvent::Progress { frame: 12, .. }) {
            assert!(kv_session::export::partial_path(&out).exists());
            job.cancel();
            ack_tx.send(()).unwrap();
        }
        if e.is_final() {
            break e;
        }
    };
    job.wait();
    assert_eq!(last, ExportEvent::Cancelled);
    assert!(!out.exists());
    assert!(!kv_session::export::partial_path(&out).exists());
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
}
