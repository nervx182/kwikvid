//! Camera import with fake card volumes in temp dirs: finding volumes,
//! listing recordings and the copy job (no real footage, no platform paths).

use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use kv_session::camera::{date_folder, partial_path, COPY_BUFFER};
use kv_session::{find_camera_volumes, list_recordings, CameraRecording, CopyEvent, CopyJob};

const TIMEOUT: Duration = Duration::from_secs(30);
/// 2024-07-14 12:00:00 UTC.
const SHOT: u64 = 1_720_958_400;

/// Write `len` bytes of a pattern to `path` with modified time [`SHOT`].
fn fake_file(path: &Path, len: usize, seed: u8) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let bytes: Vec<u8> = (0..len).map(|i| (i as u8).wrapping_add(seed)).collect();
    fs::write(path, bytes).unwrap();
    File::options()
        .write(true)
        .open(path)
        .unwrap()
        .set_modified(UNIX_EPOCH + Duration::from_secs(SHOT))
        .unwrap();
}

/// `roots/CARD` (a GoPro card) and `roots/Backup` (no DCIM).
fn fake_roots(dir: &Path) -> PathBuf {
    let roots = dir.join("roots");
    let gopro = roots.join("CARD/DCIM/100GOPRO");
    fake_file(&gopro.join("GX010001.MP4"), 300 << 10, 1);
    fake_file(&gopro.join("GX020001.MP4"), 200 << 10, 2);
    fake_file(&gopro.join("GX010001.LRV"), 10 << 10, 3);
    fake_file(&gopro.join("GX010001.THM"), 1 << 10, 4);
    fake_file(&gopro.join("GOPR0002.MP4"), 100 << 10, 5);
    fake_file(&roots.join("Backup/notes.txt"), 10, 6);
    fake_file(&roots.join("Backup/clip.mp4"), 10, 7);
    roots
}

/// Run a copy to the end and return its events.
fn copy_all(recordings: Vec<CameraRecording>, library: &Path) -> Vec<CopyEvent> {
    let (tx, rx) = mpsc::channel();
    let job = CopyJob::start(recordings, library.to_path_buf(), move |e| {
        let _ = tx.send(e);
    });
    let mut events = Vec::new();
    loop {
        let e = rx.recv_timeout(TIMEOUT).expect("copy event");
        let last = e.is_final();
        events.push(e);
        if last {
            break;
        }
    }
    job.wait();
    events
}

fn done(events: &[CopyEvent]) -> (Vec<PathBuf>, Vec<PathBuf>) {
    match events.last() {
        Some(CopyEvent::Done { copied, skipped }) => (copied.clone(), skipped.clone()),
        other => panic!("not done: {other:?}"),
    }
}

/// Events start with `Started`, progress never goes back and ends at the
/// total; returns the total.
fn check_order(events: &[CopyEvent], files: usize) -> u64 {
    let CopyEvent::Started {
        total_bytes,
        files: n,
    } = events[0]
    else {
        panic!("first event: {:?}", events[0]);
    };
    assert_eq!(n, files);
    let mut last = 0;
    for e in &events[1..events.len() - 1] {
        let CopyEvent::Progress {
            bytes_done,
            total_bytes: t,
            ..
        } = e
        else {
            panic!("unexpected {e:?}");
        };
        assert_eq!(*t, total_bytes);
        assert!(*bytes_done >= last && *bytes_done <= total_bytes);
        last = *bytes_done;
    }
    assert_eq!(last, total_bytes);
    total_bytes
}

#[test]
fn finds_the_card_and_lists_grouped_recordings() {
    let dir = tempfile::tempdir().unwrap();
    let roots = fake_roots(dir.path());
    let missing = dir.path().join("nope");
    let volumes = find_camera_volumes(&[missing, roots.clone()]);
    assert_eq!(volumes.len(), 1, "{volumes:?}");
    assert_eq!(volumes[0].name, "CARD");
    assert_eq!(volumes[0].path, roots.join("CARD"));
    assert!(volumes[0].is_gopro);

    let recs = list_recordings(&volumes[0].path);
    let gopro = roots.join("CARD/DCIM/100GOPRO");
    assert_eq!(recs.len(), 2, "{recs:?}");
    assert_eq!(recs[0].files, vec![gopro.join("GOPR0002.MP4")]);
    assert_eq!(recs[0].total_bytes, 100 << 10);
    assert_eq!(
        recs[1].files,
        vec![gopro.join("GX010001.MP4"), gopro.join("GX020001.MP4")]
    );
    assert_eq!(recs[1].total_bytes, 500 << 10);
    assert!(!recs[1].has_gaps);
    assert_eq!(
        recs[1].modified,
        Some(UNIX_EPOCH + Duration::from_secs(SHOT))
    );
}

#[test]
fn other_camera_folders_and_gaps() {
    let dir = tempfile::tempdir().unwrap();
    let roots = dir.path().join("roots");
    // A non-GoPro camera, lower-case names, a chapter missing elsewhere.
    fake_file(&roots.join("CANON/dcim/100CANON/img_0001.mov"), 10, 0);
    fake_file(&roots.join("CANON/dcim/empty/readme.txt"), 10, 0);
    fake_file(&roots.join("GP/DCIM/101GOPRO/GH020007.MP4"), 10, 0);
    // DCIM without videos: not a camera.
    fake_file(&roots.join("PHOTOS/DCIM/100PHOTO/a.jpg"), 10, 0);
    let volumes = find_camera_volumes(&[roots.clone()]);
    let names: Vec<_> = volumes
        .iter()
        .map(|v| (v.name.as_str(), v.is_gopro))
        .collect();
    assert_eq!(names, vec![("CANON", false), ("GP", true)]);
    let canon = list_recordings(&roots.join("CANON"));
    assert_eq!(canon.len(), 1);
    assert!(!canon[0].has_gaps);
    let gp = list_recordings(&roots.join("GP"));
    assert!(gp[0].has_gaps);
}

#[test]
fn copies_into_date_folders_then_resumes_and_never_overwrites() {
    let dir = tempfile::tempdir().unwrap();
    let roots = fake_roots(dir.path());
    let library = dir.path().join("library");
    let recs = list_recordings(&roots.join("CARD"));
    let day = library.join(date_folder(UNIX_EPOCH + Duration::from_secs(SHOT)));
    assert!(day.ends_with("2024-07-14"));

    let events = copy_all(recs.clone(), &library);
    assert_eq!(check_order(&events, 3), 600 << 10);
    let (copied, skipped) = done(&events);
    let expected: Vec<PathBuf> = ["GOPR0002.MP4", "GX010001.MP4", "GX020001.MP4"]
        .iter()
        .map(|n| day.join(n))
        .collect();
    assert_eq!(copied, expected);
    assert!(skipped.is_empty());
    let gopro = roots.join("CARD/DCIM/100GOPRO");
    for (dest, src) in copied.iter().zip(recs.iter().flat_map(|r| &r.files)) {
        assert_eq!(fs::read(dest).unwrap(), fs::read(src).unwrap());
        assert!(!partial_path(dest).exists());
    }
    assert!(!day.join("GX010001.LRV").exists());
    assert_eq!(fs::read_dir(&day).unwrap().count(), 3);

    // Second run: everything is already there.
    let events = copy_all(recs.clone(), &library);
    check_order(&events, 3);
    let (copied2, skipped2) = done(&events);
    assert_eq!(copied2, expected);
    assert_eq!(skipped2, expected);
    assert_eq!(fs::read_dir(&day).unwrap().count(), 3);

    // A different file with the same name: copied beside it, not over it.
    fs::write(day.join("GOPR0002.MP4"), b"something else").unwrap();
    let events = copy_all(recs[..1].to_vec(), &library);
    let (copied3, skipped3) = done(&events);
    let renamed = day.join("GOPR0002 (2).MP4");
    assert_eq!(copied3, vec![renamed.clone()]);
    assert!(skipped3.is_empty());
    assert_eq!(
        fs::read(day.join("GOPR0002.MP4")).unwrap(),
        b"something else"
    );
    assert_eq!(
        fs::read(&renamed).unwrap(),
        fs::read(gopro.join("GOPR0002.MP4")).unwrap()
    );
    // ...and found again on the next run.
    let (_, skipped4) = done(&copy_all(recs[..1].to_vec(), &library));
    assert_eq!(skipped4, vec![renamed]);
}

#[test]
fn cancel_removes_the_partial_file() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("card/DCIM/100GOPRO/GX010003.MP4");
    // Three buffers, so the copy is cancelled part-way through.
    fake_file(&src, 3 * COPY_BUFFER, 9);
    let library = dir.path().join("library");
    let recs = list_recordings(&dir.path().join("card"));
    let dest = library
        .join(date_folder(
            SystemTime::UNIX_EPOCH + Duration::from_secs(SHOT),
        ))
        .join("GX010003.MP4");

    let (tx, rx) = mpsc::channel();
    let (go_tx, go_rx) = mpsc::channel::<()>();
    let partial = partial_path(&dest);
    let job = CopyJob::start(recs, library, move |e| {
        let first_progress = matches!(e, CopyEvent::Progress { .. });
        let partial_exists = partial.exists();
        let _ = tx.send((e, partial_exists));
        if first_progress {
            // Hold the copy thread until the test has cancelled.
            let _ = go_rx.recv_timeout(TIMEOUT);
        }
    });
    let mut events = Vec::new();
    loop {
        let (e, partial_exists) = rx.recv_timeout(TIMEOUT).expect("copy event");
        if matches!(e, CopyEvent::Progress { .. }) && events.len() == 1 {
            assert!(partial_exists, "the copy writes a partial file");
            job.cancel();
            go_tx.send(()).unwrap();
        }
        let last = e.is_final();
        events.push(e);
        if last {
            break;
        }
    }
    job.wait();
    assert_eq!(events.last(), Some(&CopyEvent::Cancelled), "{events:?}");
    assert!(!partial_path(&dest).exists());
    assert!(!dest.exists());
}

#[test]
fn a_vanished_file_fails_before_copying() {
    let dir = tempfile::tempdir().unwrap();
    let roots = fake_roots(dir.path());
    let library = dir.path().join("library");
    let recs = list_recordings(&roots.join("CARD"));
    // The card is pulled after listing.
    fs::remove_file(&recs[1].files[1]).unwrap();
    let events = copy_all(recs, &library);
    let Some(CopyEvent::Failed { message }) = events.last() else {
        panic!("{events:?}");
    };
    assert!(message.contains("GX020001.MP4"), "{message}");
    assert!(matches!(events[0], CopyEvent::Started { .. }));
}

#[test]
fn a_write_error_fails_and_keeps_finished_files() {
    let dir = tempfile::tempdir().unwrap();
    let roots = fake_roots(dir.path());
    let library = dir.path().join("library");
    let recs = list_recordings(&roots.join("CARD"));
    let day = library.join(date_folder(UNIX_EPOCH + Duration::from_secs(SHOT)));
    // A folder where the second file's partial file goes: it can't be written.
    fs::create_dir_all(partial_path(&day.join("GX010001.MP4"))).unwrap();
    let events = copy_all(recs, &library);
    let Some(CopyEvent::Failed { message }) = events.last() else {
        panic!("{events:?}");
    };
    assert!(message.contains("GX010001.MP4"), "{message}");
    assert_eq!(
        fs::metadata(day.join("GOPR0002.MP4")).unwrap().len(),
        100 << 10
    );
    assert!(!day.join("GX010001.MP4").exists());
    assert!(!day.join("GX020001.MP4").exists());
}
