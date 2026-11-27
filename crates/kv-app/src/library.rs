//! Media library IPC (`ui/js/library.js`): import from paths or a
//! file/folder picker, the filmstrip jobs that follow, their `kv://job`
//! events and the `kvthumb://<media_id>/<index>` image protocol (ADR 0007).
//!
//! - `import(paths)` imports files and folders (also used for files dropped
//!   on the window) and returns the [`ImportReport`]; `pick_media(folders)`
//!   opens the native picker first and returns `null` when it is cancelled.
//!   Both emit `kv://state` with the new snapshot and queue one
//!   [`Job::Filmstrip`] per added item.
//! - After New and Open, [`Library::ensure_filmstrips`] queues the filmstrip
//!   of every video item that has no thumbnails yet and whose files exist
//!   (the cache makes this quick for a reopened project). An item whose
//!   files already have a filmstrip job in flight is not queued twice.
//! - Every job event is forwarded as a [`JobUpdate`] on `kv://job`; a
//!   finished filmstrip is stored in the session first, so the UI can load
//!   the images as soon as it sees `"done"`.
//! - `kvthumb://<media_id>/<index>` answers with the JPEG from
//!   [`Session::thumbnail`], or 404 while it has not been generated.
//!
//! Removing an item is a plain `Command::RemoveMedia` through `apply`.
//! Thumbnails are kept in the session by path, so undo brings the images
//! back with the item.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};

use kv_cache::Cache;
use kv_core::MediaId;
use kv_media::{FfmpegBackend, MediaBackend};
use kv_session::{
    AudioAnalyzer, ImportReport, Job, JobContext, JobEvent, JobMessage, JobRunner, Session,
    Snapshot,
};
use serde::Serialize;
use tauri::http::{header, Response, StatusCode};
use tauri::{AppHandle, Emitter, Manager, Runtime};
use tauri_plugin_dialog::DialogExt;

use crate::ipc::{AppState, IpcError, STATE_EVENT};

/// The event carrying a [`JobUpdate`] for every background job event.
pub const JOB_EVENT: &str = "kv://job";
/// The custom protocol serving filmstrip thumbnails.
pub const THUMB_SCHEME: &str = "kvthumb";

/// One job event as the UI sees it (`kv://job`).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct JobUpdate {
    pub id: u64,
    /// `"filmstrip"`, `"probe"`, `"hilights"` or `"music-analysis"`.
    pub kind: &'static str,
    /// The library item a filmstrip job works for.
    pub media_id: Option<MediaId>,
    /// `"queued"`, `"started"`, `"progress"`, `"done"`, `"failed"` or
    /// `"cancelled"`; the last three are final.
    pub state: &'static str,
    /// `0.0..=1.0`, with `"progress"`.
    pub fraction: Option<f32>,
    /// Why it failed, with `"failed"`.
    pub message: Option<String>,
}

impl JobUpdate {
    fn new(id: u64, job: &Job, state: &'static str) -> JobUpdate {
        let (kind, media_id) = match job {
            Job::Filmstrip { media_id, .. } => (kv_session::FILMSTRIP_KIND, Some(*media_id)),
            Job::ProbeMedia(_) => (kv_session::PROBE_KIND, None),
            Job::ReadHilights(_) => (kv_session::HILIGHTS_KIND, None),
            Job::AnalyzeMusic(_) => (kv_session::MUSIC_ANALYSIS_KIND, None),
        };
        JobUpdate {
            id,
            kind,
            media_id,
            state,
            fraction: None,
            message: None,
        }
    }

    /// The update for a job just submitted.
    pub fn queued(id: kv_session::JobId, job: &Job) -> JobUpdate {
        JobUpdate::new(id.0, job, "queued")
    }

    /// The update for a message from the [`JobRunner`].
    pub fn from_message(msg: &JobMessage) -> JobUpdate {
        let state = match &msg.event {
            JobEvent::Started => "started",
            JobEvent::Progress { .. } => "progress",
            JobEvent::Done { .. } => "done",
            JobEvent::Failed { .. } => "failed",
            JobEvent::Cancelled => "cancelled",
        };
        let mut u = JobUpdate::new(msg.id.0, &msg.job, state);
        match &msg.event {
            JobEvent::Progress { fraction } => u.fraction = Some(*fraction),
            JobEvent::Failed { message } => u.message = Some(message.clone()),
            _ => {}
        }
        u
    }
}

/// Tauri managed state: the backends import uses and the job runner, or
/// why import is unavailable (no FFmpeg).
pub struct Library {
    jobs: Result<(JobContext, JobRunner), String>,
    /// The files (host paths) of the filmstrip jobs queued and not yet
    /// final, so the same filmstrip is not queued twice. Paths, not media
    /// ids: ids are reused by the next opened project.
    pending: Mutex<HashSet<Vec<PathBuf>>>,
}

impl Library {
    /// A library over `ctx`, and the receiving end of its job events.
    pub fn new(ctx: JobContext) -> (Library, Receiver<JobMessage>) {
        let (runner, events) = JobRunner::new(ctx.clone());
        (
            Library {
                jobs: Ok((ctx, runner)),
                pending: Mutex::default(),
            },
            events,
        )
    }

    /// A library that refuses to import, saying `why`.
    pub fn unavailable(why: String) -> Library {
        Library {
            jobs: Err(why),
            pending: Mutex::default(),
        }
    }

    /// The real backends (FFmpeg, the on-disk cache in the app's cache
    /// folder) and a thread that feeds job events to the session and emits
    /// `kv://job`.
    pub fn start<R: Runtime>(app: &AppHandle<R>) -> Library {
        let ctx = match real_context(app.path().app_cache_dir().ok()) {
            Ok(ctx) => ctx,
            Err(why) => {
                tracing::warn!("{why}");
                return Library::unavailable(why);
            }
        };
        let (library, events) = Library::new(ctx);
        let app = app.clone();
        std::thread::Builder::new()
            .name("kv-job-events".into())
            .spawn(move || {
                for msg in events {
                    let mut update = JobUpdate::from_message(&msg);
                    if let Ok(mut session) = app.state::<AppState>().session() {
                        session.handle_job_message(&msg);
                        crate::music::on_job_message(&app, &mut session, &msg, &mut update);
                    }
                    // After storing the thumbnails, so a concurrent
                    // `ensure_filmstrips` sees either them or the pending job.
                    if let Some(library) = app.try_state::<Library>() {
                        library.job_finished(&msg);
                    }
                    emit(&app, JOB_EVENT, &update);
                }
            })
            .expect("failed to spawn the job event thread");
        library
    }

    fn jobs(&self) -> Result<&(JobContext, JobRunner), IpcError> {
        self.jobs.as_ref().map_err(|why| IpcError {
            message: why.clone(),
        })
    }

    /// Queue one job; its queued update (not emitted).
    pub(crate) fn submit(&self, job: Job) -> Result<JobUpdate, IpcError> {
        let (_, runner) = self.jobs()?;
        Ok(JobUpdate::queued(runner.submit(job.clone()), &job))
    }

    /// Queue the given filmstrip jobs, skipping any whose files already
    /// have one pending; the updates to emit.
    fn submit_filmstrips(&self, jobs: Vec<Job>) -> Vec<JobUpdate> {
        let Ok((_, runner)) = &self.jobs else {
            return Vec::new();
        };
        let mut pending = self.lock_pending();
        jobs.into_iter()
            .filter(|job| match filmstrip_key(job) {
                Some(key) => pending.insert(key),
                None => true,
            })
            .map(|job| JobUpdate::queued(runner.submit(job.clone()), &job))
            .collect()
    }

    /// Import `paths` into the session and queue the added items'
    /// filmstrips: the report, the new snapshot and the `queued` updates
    /// (neither emitted). Blocking: probing reads the files.
    pub(crate) fn import_paths(
        &self,
        state: &AppState,
        paths: &[PathBuf],
    ) -> Result<(ImportReport, Snapshot, Vec<JobUpdate>), IpcError> {
        let (ctx, _) = self.jobs()?;
        let (report, snapshot, jobs) = import_into(&mut *state.session()?, ctx, paths)?;
        Ok((report, snapshot, self.submit_filmstrips(jobs)))
    }

    /// Queue the filmstrip of every video item in `session` that has no
    /// thumbnails and whose files all exist (e.g. after New or Open); the
    /// updates to emit. Items with a pending filmstrip job are skipped.
    pub fn ensure_filmstrips(&self, session: &Session) -> Vec<JobUpdate> {
        let jobs = session
            .project()
            .media
            .iter()
            .filter(|item| session.thumbnail(item.id, 0).is_none())
            .filter_map(|item| session.filmstrip_job(item.id))
            .filter(|job| match job {
                Job::Filmstrip { files, .. } => {
                    !files.is_empty() && files.iter().all(|(p, _)| p.is_file())
                }
                _ => false,
            })
            .collect();
        self.submit_filmstrips(jobs)
    }

    /// Forget a filmstrip job once it is final (done, failed or cancelled),
    /// so a later [`Library::ensure_filmstrips`] may queue it again.
    pub(crate) fn job_finished(&self, msg: &JobMessage) {
        if !msg.event.is_final() {
            return;
        }
        if let Some(key) = filmstrip_key(&msg.job) {
            self.lock_pending().remove(&key);
        }
    }

    fn lock_pending(&self) -> std::sync::MutexGuard<'_, HashSet<Vec<PathBuf>>> {
        // A set of paths; a panic elsewhere cannot leave it invalid.
        self.pending.lock().unwrap_or_else(|p| p.into_inner())
    }
}

/// What identifies a filmstrip job in [`Library::pending`]: its files.
fn filmstrip_key(job: &Job) -> Option<Vec<PathBuf>> {
    match job {
        Job::Filmstrip { files, .. } => Some(files.iter().map(|(p, _)| p.clone()).collect()),
        _ => None,
    }
}

/// FFmpeg plus the cache file `cache.sqlite` in `cache_dir` (in memory when
/// it can't be opened).
fn real_context(cache_dir: Option<PathBuf>) -> Result<JobContext, String> {
    let media: Arc<dyn MediaBackend> =
        Arc::new(FfmpegBackend::new().map_err(|e| format!("Video import is unavailable: {e}"))?);
    let on_disk = cache_dir.and_then(|dir| {
        std::fs::create_dir_all(&dir)
            .map_err(|e| e.to_string())
            .and_then(|()| Cache::open(dir.join("cache.sqlite")).map_err(|e| e.to_string()))
            .inspect_err(|e| tracing::warn!(?dir, error = %e, "no cache file; caching in memory"))
            .ok()
    });
    let cache = match on_disk {
        Some(c) => c,
        None => Cache::in_memory().map_err(|e| format!("Video import is unavailable: {e}"))?,
    };
    Ok(JobContext::new(
        media,
        Arc::new(AudioAnalyzer),
        Arc::new(cache),
    ))
}

/// Import `paths` into the session: the report, the new snapshot, and the
/// filmstrip jobs of the added items (not yet submitted).
pub fn import_into(
    session: &mut Session,
    ctx: &JobContext,
    paths: &[PathBuf],
) -> Result<(ImportReport, Snapshot, Vec<Job>), IpcError> {
    let (report, snapshot) = session.import(paths, ctx)?;
    let jobs = report
        .added
        .iter()
        .filter_map(|id| session.filmstrip_job(*id))
        .collect();
    Ok((report, snapshot, jobs))
}

fn emit<R: Runtime, T: Serialize + Clone>(app: &AppHandle<R>, event: &str, payload: &T) {
    if let Err(e) = app.emit(event, payload) {
        tracing::warn!(error = %e, "could not emit {event}");
    }
}

/// Emit one `kv://job` update.
pub(crate) fn emit_job<R: Runtime>(app: &AppHandle<R>, update: &JobUpdate) {
    emit(app, JOB_EVENT, update);
}

/// Import on a blocking thread (probing reads files), then emit the new
/// state and the queued filmstrip jobs.
async fn import_and_notify<R: Runtime>(
    app: AppHandle<R>,
    paths: Vec<PathBuf>,
) -> Result<ImportReport, IpcError> {
    let worker = app.clone();
    let (report, snapshot, updates) = tauri::async_runtime::spawn_blocking(move || {
        worker
            .state::<Library>()
            .import_paths(&worker.state::<AppState>(), &paths)
    })
    .await
    .map_err(|e| IpcError {
        message: format!("Import stopped unexpectedly: {e}"),
    })??;
    emit(&app, STATE_EVENT, &snapshot);
    for u in &updates {
        emit(&app, JOB_EVENT, u);
    }
    Ok(report)
}

/// Import files and folders (absolute paths), e.g. dropped on the window.
#[tauri::command]
pub async fn import(app: AppHandle, paths: Vec<PathBuf>) -> Result<ImportReport, IpcError> {
    import_and_notify(app, paths).await
}

/// Let the user pick video files (or folders, with `folders`) and import
/// them; `None` when the picker is cancelled.
#[tauri::command]
pub async fn pick_media(app: AppHandle, folders: bool) -> Result<Option<ImportReport>, IpcError> {
    // Fail before showing a picker whose choice could not be imported.
    app.state::<Library>().jobs()?;
    let dialog = app.clone();
    let picked = tauri::async_runtime::spawn_blocking(move || {
        let picker = dialog.dialog().file().set_title(if folders {
            "Import folders"
        } else {
            "Import videos"
        });
        let chosen = if folders {
            picker.blocking_pick_folders()
        } else {
            let exts: Vec<String> = kv_session::IMPORT_EXTENSIONS
                .iter()
                .flat_map(|e| [e.to_string(), e.to_uppercase()])
                .collect();
            let exts: Vec<&str> = exts.iter().map(String::as_str).collect();
            picker.add_filter("Videos", &exts).blocking_pick_files()
        };
        chosen
            .unwrap_or_default()
            .into_iter()
            .filter_map(|p| p.into_path().ok())
            .collect::<Vec<_>>()
    })
    .await
    .map_err(|e| IpcError {
        message: format!("The file picker failed: {e}"),
    })?;
    if picked.is_empty() {
        return Ok(None);
    }
    import_and_notify(app, picked).await.map(Some)
}

/// The `(media_id, index)` a `kvthumb` URL names: `kvthumb://3/7` (macOS,
/// Linux) or `http://kvthumb.localhost/3/7` (Windows).
pub fn parse_thumb_uri(uri: &tauri::http::Uri) -> Option<(MediaId, usize)> {
    let host = uri.host().unwrap_or("");
    let path = uri.path().trim_matches('/');
    let rest = if host.ends_with(".localhost") || host.is_empty() {
        path.to_string()
    } else {
        format!("{host}/{path}")
    };
    let mut parts = rest.split('/');
    let id = parts.next()?.parse().ok()?;
    let index = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some((MediaId(id), index))
}

/// The protocol's answer to `uri`: 200 with the JPEG, 404 while it is not
/// generated (or the item is gone), 400 for a malformed URL.
pub fn thumb_response(session: &Session, uri: &tauri::http::Uri) -> Response<Vec<u8>> {
    let (status, content_type, body) = match parse_thumb_uri(uri) {
        None => (
            StatusCode::BAD_REQUEST,
            "text/plain",
            b"bad thumbnail URL".to_vec(),
        ),
        Some((id, index)) => match session.thumbnail(id, index) {
            Some(jpeg) => (StatusCode::OK, "image/jpeg", jpeg),
            None => (
                StatusCode::NOT_FOUND,
                "text/plain",
                b"not generated yet".to_vec(),
            ),
        },
    };
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, content_type)
        // Item ids can be reused after an undo, and a 404 turns into an image
        // once the job finishes: never serve a stale answer.
        .header(header::CACHE_CONTROL, "no-store")
        .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
        .body(body)
        .expect("valid thumbnail response")
}

/// Register the `kvthumb` protocol and the dialog plugin on `builder`.
pub fn register<R: Runtime>(builder: tauri::Builder<R>) -> tauri::Builder<R> {
    builder
        .plugin(tauri_plugin_dialog::init())
        .register_asynchronous_uri_scheme_protocol(THUMB_SCHEME, |ctx, request, responder| {
            let app = ctx.app_handle().clone();
            let uri = request.uri().clone();
            // Off the webview thread: the session may be locked by an import.
            std::thread::spawn(move || {
                let response = match app.state::<AppState>().session() {
                    Ok(session) => thumb_response(&session, &uri),
                    Err(e) => Response::builder()
                        .status(StatusCode::INTERNAL_SERVER_ERROR)
                        .header(header::CONTENT_TYPE, "text/plain")
                        .body(e.message.into_bytes())
                        .expect("valid error response"),
                };
                responder.respond(response);
            });
        })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use kv_audio::MusicAnalysis;
    use kv_core::{Command, Micros};
    use kv_media::{DecodeOptions, MediaError, MediaInfo, Rational, VideoInfo, VideoReader};
    use kv_session::{FilmstripGenerator, JobId, MusicAnalyzer, FILMSTRIP_COUNT};
    use std::error::Error;
    use std::path::Path;
    use std::time::Duration;
    use tauri::http::Uri;

    /// Every file is a 2 s video.
    struct FakeMedia;

    impl MediaBackend for FakeMedia {
        fn probe(&self, _: &Path) -> kv_media::Result<MediaInfo> {
            Ok(MediaInfo {
                duration: Micros(2_000_000),
                container: "fake".into(),
                video: Some(VideoInfo {
                    codec: "fake".into(),
                    width: 320,
                    height: 240,
                    fps: Rational::new(30, 1),
                    bit_depth: 8,
                    rotation: 0,
                }),
                audio: None,
                creation_time: None,
            })
        }

        fn open_video(
            &self,
            p: &Path,
            _: &DecodeOptions,
        ) -> kv_media::Result<Box<dyn VideoReader>> {
            Err(MediaError::NoVideoStream(p.to_path_buf()))
        }
    }

    struct NoMusic;

    impl MusicAnalyzer for NoMusic {
        fn analyze(&self, _: &Path) -> Result<MusicAnalysis, Box<dyn Error + Send + Sync>> {
            Err("no music".into())
        }
    }

    /// Thumbnail `i` is the bytes `[0xFF, 0xD8, i]`.
    struct FakeFilmstrips;

    impl FilmstripGenerator for FakeFilmstrips {
        fn generate(
            &self,
            _: &[(PathBuf, Micros)],
            count: u32,
            _: u32,
        ) -> Result<Vec<Vec<u8>>, Box<dyn Error + Send + Sync>> {
            Ok((0..count).map(|i| vec![0xFF, 0xD8, i as u8]).collect())
        }
    }

    /// Fake backends: 2 s videos, no music, tiny thumbnails.
    pub(crate) fn ctx() -> JobContext {
        JobContext {
            media: Arc::new(FakeMedia),
            analyzer: Arc::new(NoMusic),
            cache: Arc::new(Cache::in_memory().unwrap()),
            filmstrips: Arc::new(FakeFilmstrips),
        }
    }

    fn uri(s: &str) -> Uri {
        s.parse().unwrap()
    }

    #[test]
    fn parses_thumb_urls_on_every_platform() {
        assert_eq!(
            parse_thumb_uri(&uri("kvthumb://3/7")),
            Some((MediaId(3), 7))
        );
        assert_eq!(
            parse_thumb_uri(&uri("http://kvthumb.localhost/3/7")),
            Some((MediaId(3), 7))
        );
        for bad in [
            "kvthumb://3",
            "kvthumb://x/1",
            "kvthumb://1/2/3",
            "kvthumb://1/-1",
        ] {
            assert_eq!(parse_thumb_uri(&uri(bad)), None, "{bad}");
        }
    }

    /// Import a file, run its filmstrip job and serve the thumbnails: 404
    /// before the job's `Done`, 200 `image/jpeg` after, 404 once removed and
    /// 200 again after undo.
    #[test]
    fn import_then_thumbnails_over_the_protocol() {
        let dir = tempfile::tempdir().unwrap();
        let clip = dir.path().join("GX010001.MP4");
        std::fs::write(&clip, b"clip").unwrap();
        let notes = dir.path().join("notes.txt");
        std::fs::write(&notes, b"notes").unwrap();
        let mut session = Session::new(dir.path().to_path_buf());
        let (library, events) = Library::new(ctx());

        let (ctx, _) = library.jobs().unwrap();
        let (report, snapshot, jobs) =
            import_into(&mut session, ctx, &[clip.clone(), notes.clone()]).unwrap();
        assert_eq!(report.added, vec![MediaId(1)]);
        assert_eq!(report.skipped.len(), 1);
        assert_eq!(report.skipped[0].0, notes);
        assert_eq!(snapshot.project.media.len(), 1);
        assert_eq!(jobs.len(), 1);

        let thumb = uri("kvthumb://1/0");
        let r = thumb_response(&session, &thumb);
        assert_eq!(r.status(), StatusCode::NOT_FOUND);
        assert_eq!(r.headers()[header::CACHE_CONTROL], "no-store");

        let updates = library.submit_filmstrips(jobs);
        assert_eq!(updates.len(), 1);
        assert_eq!(updates[0].state, "queued");
        assert_eq!(updates[0].kind, "filmstrip");
        assert_eq!(updates[0].media_id, Some(MediaId(1)));
        loop {
            let msg = events.recv_timeout(Duration::from_secs(10)).unwrap();
            session.handle_job_message(&msg);
            if msg.event.is_final() {
                assert_eq!(JobUpdate::from_message(&msg).state, "done");
                break;
            }
        }

        let r = thumb_response(&session, &thumb);
        assert_eq!(r.status(), StatusCode::OK);
        assert_eq!(r.headers()[header::CONTENT_TYPE], "image/jpeg");
        assert_eq!(r.body(), &vec![0xFF, 0xD8, 0]);
        let last = uri(&format!("kvthumb://1/{}", FILMSTRIP_COUNT - 1));
        assert_eq!(thumb_response(&session, &last).status(), StatusCode::OK);
        let past = uri(&format!("kvthumb://1/{FILMSTRIP_COUNT}"));
        assert_eq!(
            thumb_response(&session, &past).status(),
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            thumb_response(&session, &uri("kvthumb://1")).status(),
            StatusCode::BAD_REQUEST
        );

        session
            .apply(Command::RemoveMedia {
                media_id: MediaId(1),
            })
            .unwrap();
        assert_eq!(
            thumb_response(&session, &thumb).status(),
            StatusCode::NOT_FOUND
        );
        session.undo().unwrap();
        assert_eq!(thumb_response(&session, &thumb).status(), StatusCode::OK);
    }

    fn video(id: u64, path: &str) -> Command {
        Command::AddMedia {
            item: kv_core::MediaItem {
                id: MediaId(id),
                path: path.into(),
                duration: Micros(2_000_000),
                kind: kv_core::MediaKind::Video,
                hilights: vec![],
                clip_audio: false,
                chapters: vec![],
            },
        }
    }

    /// Feed job events to the session and the library until `n` jobs are
    /// final, as the app's job event thread does.
    fn finish_jobs(n: usize, events: &Receiver<JobMessage>, state: &AppState, library: &Library) {
        let mut done = 0;
        while done < n {
            let msg = events.recv_timeout(Duration::from_secs(10)).unwrap();
            state.session().unwrap().handle_job_message(&msg);
            library.job_finished(&msg);
            if msg.event.is_final() {
                assert_eq!(JobUpdate::from_message(&msg).state, "done");
                done += 1;
            }
        }
    }

    /// Open a project (as `open_project` does): one filmstrip job is queued
    /// per video item whose file exists, none for a missing file, none again
    /// while pending, and none once the thumbnails are in.
    #[test]
    fn opened_project_queues_missing_filmstrips_once() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.mp4"), b"a").unwrap();
        std::fs::write(dir.path().join("b.mp4"), b"b").unwrap();
        let mut other = Session::new(dir.path().to_path_buf());
        other.apply(video(1, "a.mp4")).unwrap();
        other.apply(video(2, "b.mp4")).unwrap();
        other.apply(video(3, "gone/c.mp4")).unwrap();
        let file = dir.path().join("trip.kwikvid");
        other.save_as(&file).unwrap();

        let state = AppState::new(Session::new(dir.path().to_path_buf()));
        let files = crate::project::ProjectFiles::load(None);
        let (library, events) = Library::new(ctx());
        let opened = crate::project::open(&state, &files, &file).unwrap();
        assert_eq!(opened.missing.len(), 1);

        let updates = crate::project::queue_filmstrips(&state, &library);
        let mut ids: Vec<_> = updates.iter().map(|u| u.media_id).collect();
        ids.sort();
        assert_eq!(ids, vec![Some(MediaId(1)), Some(MediaId(2))]);
        assert!(updates
            .iter()
            .all(|u| u.state == "queued" && u.kind == "filmstrip"));
        // Still pending: not queued again.
        assert!(crate::project::queue_filmstrips(&state, &library).is_empty());

        finish_jobs(2, &events, &state, &library);
        let thumb = uri("kvthumb://2/0");
        assert_eq!(
            thumb_response(&state.session().unwrap(), &thumb).status(),
            StatusCode::OK
        );
        // The thumbnails are in: nothing to queue.
        assert!(crate::project::queue_filmstrips(&state, &library).is_empty());

        // Reopening drops the session's thumbnails: queued again (the cache
        // answers quickly).
        crate::project::open(&state, &files, &file).unwrap();
        assert_eq!(crate::project::queue_filmstrips(&state, &library).len(), 2);
        finish_jobs(2, &events, &state, &library);
    }

    /// An item queued by import is not queued again by `ensure_filmstrips`.
    #[test]
    fn imported_item_with_a_pending_job_is_not_queued_twice() {
        let dir = tempfile::tempdir().unwrap();
        let clip = dir.path().join("GX010001.MP4");
        std::fs::write(&clip, b"clip").unwrap();
        let mut session = Session::new(dir.path().to_path_buf());
        let (library, events) = Library::new(ctx());
        let (ctx, _) = library.jobs().unwrap();
        let (_, _, jobs) = import_into(&mut session, ctx, &[clip]).unwrap();
        assert_eq!(library.submit_filmstrips(jobs).len(), 1);
        assert!(library.ensure_filmstrips(&session).is_empty());
        // Once final (here: done), it may be queued again if needed.
        let msg = loop {
            let msg = events.recv_timeout(Duration::from_secs(10)).unwrap();
            if msg.event.is_final() {
                break msg;
            }
        };
        library.job_finished(&msg);
        assert_eq!(library.ensure_filmstrips(&session).len(), 1);
    }

    #[test]
    fn unavailable_library_refuses_with_its_reason() {
        let library = Library::unavailable("Video import is unavailable: no FFmpeg".into());
        assert_eq!(
            library.jobs().unwrap_err().message,
            "Video import is unavailable: no FFmpeg"
        );
        assert!(library.submit_filmstrips(Vec::new()).is_empty());
    }

    #[test]
    fn job_updates_serialise_for_the_ui() {
        let job = Job::Filmstrip {
            media_id: MediaId(4),
            files: Vec::new(),
        };
        let msg = |event| JobMessage {
            id: JobId(9),
            job: job.clone(),
            event,
        };
        let json = |u: JobUpdate| serde_json::to_value(u).unwrap();
        assert_eq!(
            json(JobUpdate::queued(JobId(9), &job)),
            serde_json::json!({"id": 9, "kind": "filmstrip", "media_id": 4, "state": "queued",
                "fraction": null, "message": null})
        );
        let p = json(JobUpdate::from_message(&msg(JobEvent::Progress {
            fraction: 0.5,
        })));
        assert_eq!(p["state"], "progress");
        assert_eq!(p["fraction"], 0.5);
        let f = json(JobUpdate::from_message(&msg(JobEvent::Failed {
            message: "Couldn't read it.".into(),
        })));
        assert_eq!(f["state"], "failed");
        assert_eq!(f["message"], "Couldn't read it.");
        let probe = JobUpdate::queued(JobId(1), &Job::ProbeMedia(PathBuf::from("/a.mp4")));
        assert_eq!((probe.kind, probe.media_id), ("probe", None));
        assert_eq!(
            JobUpdate::from_message(&msg(JobEvent::Cancelled)).state,
            "cancelled"
        );
    }

    /// The UI tests' `import-report.json` has the shape `import` returns:
    /// `skipped` is a list of `[path, reason]` pairs.
    #[test]
    fn import_report_serialises_as_the_ui_fixture_expects() {
        let r = ImportReport {
            added: vec![MediaId(1)],
            skipped: vec![(PathBuf::from("/home/ana/x.jpg"), "Nope.".into())],
        };
        assert_eq!(
            serde_json::to_value(&r).unwrap(),
            serde_json::json!({"added": [1], "skipped": [["/home/ana/x.jpg", "Nope."]]})
        );
    }
}
