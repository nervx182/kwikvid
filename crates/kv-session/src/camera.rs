//! Camera import: find mounted camera / SD card volumes, list their
//! recordings and copy them into a library folder.
//!
//! Editing straight off a card is slow and breaks when it is ejected, so
//! footage is copied first; the caller then passes [`CopyEvent::Done`]'s
//! `copied` files to [`Session::import`](crate::Session::import).
//!
//! # Volumes and folder layout
//!
//! A camera card (or a camera in USB mass-storage mode) mounts as a disk
//! under one of [`default_mount_roots`]. Its videos are in subfolders of
//! `DCIM`; a GoPro card looks like this:
//!
//! ```text
//! /Volumes/GOPRO/DCIM/100GOPRO/GX010042.MP4   chapter 1 of recording 42
//!                              GX020042.MP4   chapter 2
//!                              GX010042.LRV   low-res proxy (not copied)
//!                              GX010042.THM   thumbnail (not copied)
//!                     101GOPRO/...
//! ```
//!
//! [`find_camera_volumes`] lists each direct child of a mount root that has
//! a `DCIM` folder with at least one subfolder holding `.mp4`/`.mov` files
//! (the boot volume link in `/Volumes` has no `DCIM`, so it is not listed).
//! [`list_recordings`] reads only file metadata (no probing), so listing a
//! large card is fast, and groups chapters with [`kv_gopro::group_chapters`].
//! Cameras that do not mount as a disk (MTP/PTP, Open GoPro) are not covered.
//!
//! # Copying
//!
//! [`CopyJob::start`] copies on its own thread, with a large buffer and
//! `std::fs` only, and reports [`CopyEvent`]s; it can be cancelled. Each
//! file goes to `<library>/<YYYY-MM-DD>/<original name>`, written as a
//! hidden `.partial` file ([`partial_path`]) and renamed when complete.
//!
//! - **Date folders** (guess): the date is the recording's first file's
//!   modified time, in **UTC**, because the standard library has no local
//!   time zone and no new dependency was wanted. All chapters of a
//!   recording use the same date, so they stay in one folder and
//!   [`Session::import`](crate::Session::import) still groups them.
//! - **Dedupe** (guess): a destination that already exists with the same
//!   size is taken to be the same file. It is skipped and still counted as
//!   copied, so re-running an interrupted import resumes it. A destination
//!   of a different size is never overwritten: the copy gets a ` (2)` (or
//!   ` (3)`, ...) suffix. Size is the only check; no hashing.
//! - **`.LRV`/`.THM` files are not copied** (guess): only
//!   [`IMPORT_EXTENSIONS`] videos. The `.LRV` proxies could later serve as
//!   preview proxies (see issue #78).
//!
//! A failed copy (disk full, card removed) removes the file it was writing
//! and keeps the files already finished. Mount roots on Windows are not
//! known yet ([`default_mount_roots`] returns none); pass roots explicitly.

use std::fs::File;
use std::io::{ErrorKind, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

pub use crate::export::partial_path;
use crate::import::has_import_extension;
#[cfg(doc)]
use crate::IMPORT_EXTENSIONS;

/// Copy buffer size: 4 MiB.
pub const COPY_BUFFER: usize = 4 << 20;
/// A progress event is sent at most this often...
const PROGRESS_INTERVAL: Duration = Duration::from_millis(100);
/// ...or after this many bytes, whichever comes first (and when a file ends).
const PROGRESS_BYTES: u64 = 1 << 20;

/// Where removable volumes are mounted on this system: `/Volumes` on macOS,
/// `/media/$USER` and `/run/media/$USER` on Linux. Windows: none yet (drive
/// letters are not scanned); the caller passes roots explicitly.
pub fn default_mount_roots() -> Vec<PathBuf> {
    if cfg!(target_os = "macos") {
        vec![PathBuf::from("/Volumes")]
    } else if cfg!(target_os = "linux") {
        match std::env::var_os("USER").filter(|u| !u.is_empty()) {
            Some(user) => vec![
                Path::new("/media").join(&user),
                Path::new("/run/media").join(&user),
            ],
            None => vec![],
        }
    } else {
        vec![]
    }
}

/// A mounted volume that looks like a camera card.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CameraVolume {
    /// The volume's mount point.
    pub path: PathBuf,
    /// Its display name (the mount point's folder name).
    pub name: String,
    /// A `DCIM` subfolder is named like `100GOPRO`.
    pub is_gopro: bool,
}

/// The camera volumes directly inside `roots`, sorted by path. Unreadable
/// roots and entries are skipped, never an error.
pub fn find_camera_volumes(roots: &[PathBuf]) -> Vec<CameraVolume> {
    let mut out = Vec::new();
    for root in roots {
        let Ok(entries) = std::fs::read_dir(root) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let folders = video_folders(&path);
            if folders.is_empty() {
                continue;
            }
            let is_gopro = folders.iter().any(|f| {
                f.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(is_gopro_folder)
            });
            out.push(CameraVolume {
                name: entry.file_name().to_string_lossy().into_owned(),
                path,
                is_gopro,
            });
        }
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out
}

/// One recording on a card: its chapter files in order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CameraRecording {
    /// The video files, in chapter order.
    pub files: Vec<PathBuf>,
    /// Their sizes added up.
    pub total_bytes: u64,
    /// The first file's modified time.
    pub modified: Option<SystemTime>,
    /// A chapter is missing ([`kv_gopro::ChapterGroup::has_gaps`]).
    pub has_gaps: bool,
}

/// The recordings on `volume`: the videos in every `DCIM/*/` folder
/// (`.LRV`/`.THM` ignored), chapters grouped, sorted by first file name.
/// Reads file metadata only. Unreadable folders and files are skipped.
pub fn list_recordings(volume: &Path) -> Vec<CameraRecording> {
    let files: Vec<PathBuf> = video_folders(volume)
        .iter()
        .flat_map(|dir| videos_in(dir))
        .collect();
    let mut out: Vec<CameraRecording> = kv_gopro::group_chapters(files)
        .into_iter()
        .map(|g| {
            let has_gaps = g.has_gaps();
            let metas: Vec<_> = g.files.iter().map(std::fs::metadata).collect();
            CameraRecording {
                total_bytes: metas.iter().flatten().map(|m| m.len()).sum(),
                modified: metas
                    .first()
                    .and_then(|m| m.as_ref().ok())
                    .and_then(|m| m.modified().ok()),
                files: g.files,
                has_gaps,
            }
        })
        .collect();
    out.sort_by(|a, b| {
        let key = |r: &CameraRecording| {
            r.files
                .first()
                .map(|f| (f.file_name().map(|n| n.to_owned()), f.clone()))
        };
        key(a).cmp(&key(b))
    });
    out
}

/// The `DCIM` subfolders of `volume` that hold at least one video, sorted.
fn video_folders(volume: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(volume) else {
        return vec![];
    };
    let mut out = Vec::new();
    for dcim in entries.flatten() {
        if !dcim.file_name().eq_ignore_ascii_case("dcim") || !dcim.path().is_dir() {
            continue;
        }
        let Ok(subs) = std::fs::read_dir(dcim.path()) else {
            continue;
        };
        for sub in subs.flatten() {
            let path = sub.path();
            if path.is_dir() && !videos_in(&path).is_empty() {
                out.push(path);
            }
        }
    }
    out.sort();
    out
}

/// The video files directly inside `dir`, sorted.
fn videos_in(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return vec![];
    };
    let mut out: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| has_import_extension(p) && p.is_file())
        .collect();
    out.sort();
    out
}

/// `NNNGOPRO`, case-insensitive.
fn is_gopro_folder(name: &str) -> bool {
    let b = name.as_bytes();
    b.len() == 8 && b[..3].iter().all(u8::is_ascii_digit) && b[3..].eq_ignore_ascii_case(b"GOPRO")
}

/// What a [`CopyJob`] reports, in order: `Started`, `Progress`
/// (repeatedly), then one of `Done`, `Failed` or `Cancelled`.
///
/// JSON is tagged by `state`, e.g.
/// `{"state":"progress","bytes_done":1048576,"total_bytes":9000000,"file":"/Volumes/GOPRO/DCIM/100GOPRO/GX010042.MP4"}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum CopyEvent {
    /// `files` is the number of files to copy.
    Started { total_bytes: u64, files: usize },
    /// Sent at most every ~100 ms or per MiB, and when each file ends.
    /// `file` is the source file being copied. Skipped files count as done.
    Progress {
        bytes_done: u64,
        total_bytes: u64,
        file: PathBuf,
    },
    /// `copied`: every destination file, ready for `Session::import`
    /// (including the skipped ones). `skipped`: the destinations that
    /// already existed with the same size.
    Done {
        copied: Vec<PathBuf>,
        skipped: Vec<PathBuf>,
    },
    /// `message` is a short, readable sentence. Files already finished stay.
    Failed { message: String },
    /// The file being written was removed; files already finished stay.
    Cancelled,
}

impl CopyEvent {
    /// Whether this is the job's last event.
    pub fn is_final(&self) -> bool {
        matches!(
            self,
            CopyEvent::Done { .. } | CopyEvent::Failed { .. } | CopyEvent::Cancelled
        )
    }
}

/// One camera import copy running on its own thread.
///
/// Dropping the handle does not stop the copy; call [`CopyJob::cancel`]
/// first.
pub struct CopyJob {
    cancel: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl std::fmt::Debug for CopyJob {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CopyJob")
            .field("cancelled", &self.cancel.load(Ordering::SeqCst))
            .finish_non_exhaustive()
    }
}

impl CopyJob {
    /// Start copying every file of `recordings` into `library_dir` (see the
    /// module docs for the layout); every [`CopyEvent`] goes to `on_event`,
    /// on the copy thread.
    pub fn start(
        recordings: Vec<CameraRecording>,
        library_dir: PathBuf,
        mut on_event: impl FnMut(CopyEvent) + Send + 'static,
    ) -> CopyJob {
        let cancel = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&cancel);
        let thread = thread::Builder::new()
            .name("kv-camera-copy".into())
            .spawn(move || {
                let event = run(&recordings, &library_dir, &flag, &mut on_event);
                tracing::info!(?event, "camera copy finished");
                on_event(event);
            })
            .expect("failed to spawn the camera copy thread");
        CopyJob {
            cancel,
            thread: Some(thread),
        }
    }

    /// Ask the copy to stop after the current buffer; it then removes the
    /// partial file and reports `Cancelled`.
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::SeqCst);
    }

    /// Whether the copy thread has sent its final event and exited.
    pub fn is_finished(&self) -> bool {
        self.thread.as_ref().is_none_or(|t| t.is_finished())
    }

    /// Block until the copy thread exits.
    pub fn wait(mut self) {
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// How one file copy ended.
enum FileOutcome {
    Copied,
    Cancelled,
}

/// Sends throttled progress events.
struct Progress<'a> {
    on_event: &'a mut dyn FnMut(CopyEvent),
    total_bytes: u64,
    bytes_done: u64,
    last_bytes: u64,
    last_time: Instant,
}

impl Progress<'_> {
    fn add(&mut self, bytes: u64, file: &Path, force: bool) {
        self.bytes_done += bytes;
        if force
            || self.bytes_done - self.last_bytes >= PROGRESS_BYTES
            || self.last_time.elapsed() >= PROGRESS_INTERVAL
        {
            self.last_bytes = self.bytes_done;
            self.last_time = Instant::now();
            (self.on_event)(CopyEvent::Progress {
                bytes_done: self.bytes_done,
                total_bytes: self.total_bytes,
                file: file.to_path_buf(),
            });
        }
    }
}

/// The copy itself (after nothing has been sent); returns the final event.
fn run(
    recordings: &[CameraRecording],
    library: &Path,
    cancel: &AtomicBool,
    on_event: &mut dyn FnMut(CopyEvent),
) -> CopyEvent {
    // (source, size, date folder) for every file.
    let mut files = Vec::new();
    for rec in recordings {
        let day = rec
            .modified
            .map(date_folder)
            .unwrap_or_else(|| "undated".into());
        for file in &rec.files {
            match std::fs::metadata(file) {
                Ok(m) => files.push((file.clone(), m.len(), day.clone())),
                Err(e) => {
                    on_event(CopyEvent::Started {
                        total_bytes: 0,
                        files: 0,
                    });
                    return failed_reading(file, &e);
                }
            }
        }
    }
    let total_bytes = files.iter().map(|f| f.1).sum();
    tracing::info!(files = files.len(), total_bytes, library = %library.display(), "camera copy started");
    on_event(CopyEvent::Started {
        total_bytes,
        files: files.len(),
    });
    let mut progress = Progress {
        on_event,
        total_bytes,
        bytes_done: 0,
        last_bytes: 0,
        last_time: Instant::now(),
    };
    let mut buf = vec![0u8; COPY_BUFFER];
    let mut copied = Vec::new();
    let mut skipped = Vec::new();
    for (src, size, day) in &files {
        if cancel.load(Ordering::SeqCst) {
            return CopyEvent::Cancelled;
        }
        let dir = library.join(day);
        if let Err(e) = std::fs::create_dir_all(&dir) {
            return CopyEvent::Failed {
                message: format!("Couldn't create the folder \"{}\": {e}", dir.display()),
            };
        }
        let (dest, exists) = destination(&dir, src, *size);
        if exists {
            tracing::debug!(src = %src.display(), dest = %dest.display(), "already copied");
            progress.add(*size, src, true);
            skipped.push(dest.clone());
            copied.push(dest);
            continue;
        }
        match copy_file(src, &dest, &mut buf, cancel, &mut progress) {
            Ok(FileOutcome::Copied) => copied.push(dest),
            Ok(FileOutcome::Cancelled) => return CopyEvent::Cancelled,
            Err(message) => return CopyEvent::Failed { message },
        }
    }
    CopyEvent::Done { copied, skipped }
}

/// Copy `src` to `dest` through its partial file. On cancel or error the
/// partial file is removed.
fn copy_file(
    src: &Path,
    dest: &Path,
    buf: &mut [u8],
    cancel: &AtomicBool,
    progress: &mut Progress<'_>,
) -> Result<FileOutcome, String> {
    let tmp = partial_path(dest);
    let result = (|| {
        let mut input = File::open(src).map_err(|e| failed_message(src, "read", &e))?;
        let mut output = File::create(&tmp).map_err(|e| failed_message(dest, "write", &e))?;
        loop {
            let n = match input.read(buf) {
                Ok(0) => break,
                Ok(n) => n,
                Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                Err(e) => return Err(failed_message(src, "read", &e)),
            };
            output
                .write_all(&buf[..n])
                .map_err(|e| failed_message(dest, "write", &e))?;
            if cancel.load(Ordering::SeqCst) {
                return Ok(FileOutcome::Cancelled);
            }
            progress.add(n as u64, src, false);
        }
        output
            .sync_all()
            .map_err(|e| failed_message(dest, "write", &e))?;
        // Keep the camera's time, so the library sorts like the card.
        if let Ok(modified) = input.metadata().and_then(|m| m.modified()) {
            let _ = output.set_modified(modified);
        }
        drop(output);
        std::fs::rename(&tmp, dest).map_err(|e| failed_message(dest, "write", &e))?;
        Ok(FileOutcome::Copied)
    })();
    match &result {
        Ok(FileOutcome::Copied) => progress.add(0, src, true),
        _ => {
            let _ = std::fs::remove_file(&tmp);
        }
    }
    result
}

/// Where `src` (of `size` bytes) goes in `dir`, and whether it is already
/// there: the original name, or the first free `name (N).ext`; an existing
/// file of the same size counts as the same file.
fn destination(dir: &Path, src: &Path, size: u64) -> (PathBuf, bool) {
    let name = src.file_name().unwrap_or_default();
    let stem = Path::new(name)
        .file_stem()
        .unwrap_or_default()
        .to_string_lossy();
    let ext = Path::new(name).extension().map(|e| e.to_string_lossy());
    for n in 1u32.. {
        let candidate = if n == 1 {
            dir.join(name)
        } else {
            match &ext {
                Some(ext) => dir.join(format!("{stem} ({n}).{ext}")),
                None => dir.join(format!("{stem} ({n})")),
            }
        };
        match std::fs::metadata(&candidate) {
            Err(_) => return (candidate, false),
            Ok(m) if m.is_file() && m.len() == size => return (candidate, true),
            Ok(_) => {}
        }
    }
    unreachable!("ran out of suffixes")
}

fn failed_reading(file: &Path, e: &std::io::Error) -> CopyEvent {
    CopyEvent::Failed {
        message: failed_message(file, "read", e),
    }
}

fn failed_message(file: &Path, verb: &str, e: &std::io::Error) -> String {
    let label = file
        .file_name()
        .unwrap_or(file.as_os_str())
        .to_string_lossy();
    if e.kind() == ErrorKind::StorageFull {
        return format!("Couldn't {verb} \"{label}\": the disk is full.");
    }
    format!("Couldn't {verb} \"{label}\": {e}")
}

/// `YYYY-MM-DD` of `time` in UTC (see the module docs).
pub fn date_folder(time: SystemTime) -> String {
    let secs = match time.duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_secs() as i64,
        Err(e) => -(e.duration().as_secs_f64().ceil() as i64),
    };
    let (y, m, d) = civil_from_days(secs.div_euclid(86_400));
    format!("{y:04}-{m:02}-{d:02}")
}

/// Gregorian date of a day count since 1970-01-01 (Howard Hinnant's
/// `civil_from_days`).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn date_folders_are_utc_days() {
        let t = |s: u64| date_folder(UNIX_EPOCH + Duration::from_secs(s));
        assert_eq!(t(0), "1970-01-01");
        assert_eq!(t(86_399), "1970-01-01");
        assert_eq!(t(951_782_400), "2000-02-29");
        assert_eq!(t(1_709_208_000), "2024-02-29");
        assert_eq!(t(1_735_689_599), "2024-12-31");
        assert_eq!(
            date_folder(UNIX_EPOCH - Duration::from_secs(1)),
            "1969-12-31"
        );
    }

    #[test]
    fn gopro_folder_names() {
        assert!(is_gopro_folder("100GOPRO"));
        assert!(is_gopro_folder("999gopro"));
        assert!(!is_gopro_folder("100MEDIA"));
        assert!(!is_gopro_folder("10GOPRO"));
        assert!(!is_gopro_folder("A00GOPRO"));
    }

    #[test]
    fn events_serialise_tagged_by_state() {
        let p = CopyEvent::Progress {
            bytes_done: 1,
            total_bytes: 2,
            file: "/c/a.mp4".into(),
        };
        let json = serde_json::to_value(&p).unwrap();
        assert_eq!(json["state"], "progress");
        assert_eq!(json["bytes_done"], 1);
        assert_eq!(
            serde_json::to_value(CopyEvent::Started {
                total_bytes: 5,
                files: 2
            })
            .unwrap(),
            serde_json::json!({ "state": "started", "total_bytes": 5, "files": 2 })
        );
        assert!(!p.is_final() && CopyEvent::Cancelled.is_final());
    }

    #[test]
    fn mount_roots_by_platform() {
        let roots = default_mount_roots();
        if cfg!(target_os = "macos") {
            assert_eq!(roots, vec![PathBuf::from("/Volumes")]);
        } else if cfg!(target_os = "linux") {
            assert!(roots.len() == 2 || roots.is_empty());
        } else {
            assert!(roots.is_empty());
        }
    }
}
