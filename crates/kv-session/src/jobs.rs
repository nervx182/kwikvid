//! Background jobs: a small thread pool that probes media, reads HiLights,
//! analyses music and makes filmstrip thumbnails, with results cached in
//! [`kv_cache`].
//!
//! [`JobRunner::submit`] queues a [`Job`] and returns its [`JobId`]. Workers
//! report on the event channel returned by [`JobRunner::new`], as
//! [`JobMessage`]s carrying a [`JobEvent`]:
//!
//! - `Started`, then zero or more `Progress`, then one of `Done`, `Failed`
//!   or `Cancelled`;
//! - or just `Cancelled`, for a job cancelled before a worker picked it up.
//!
//! Every job first looks up its result in the cache (keyed by the file's
//! path, size and mtime, plus a `kind` and versioned `params`, see the
//! `*_KIND` / `*_PARAMS` constants) and stores a freshly computed result. A
//! cache that fails to read or write is logged and bypassed, never an error.
//!
//! Plain `std` threads and channels; no async runtime.

use std::collections::HashMap;
use std::fmt;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;

use kv_audio::MusicAnalysis;
use kv_cache::{Cache, SourceKey};
use kv_core::{MediaId, Micros};
use kv_media::{MediaBackend, MediaInfo};
use serde::de::DeserializeOwned;
use serde::Serialize;

use crate::analysis::MusicAnalyzer;
use crate::filmstrips::{FilmstripGenerator, FILMSTRIP_COUNT, FILMSTRIP_HEIGHT};

/// Cache `kind` of [`Job::AnalyzeMusic`] results.
pub const MUSIC_ANALYSIS_KIND: &str = "music-analysis";
/// Cache `params` of [`Job::AnalyzeMusic`] results. Bump the version when
/// `kv_audio`'s algorithm or `MusicAnalysis` changes.
pub const MUSIC_ANALYSIS_PARAMS: &str = "v=1";
/// Cache `kind` of [`Job::ProbeMedia`] results.
pub const PROBE_KIND: &str = "probe";
/// Cache `params` of [`Job::ProbeMedia`] results.
pub const PROBE_PARAMS: &str = "v=1";
/// Cache `kind` of [`Job::ReadHilights`] results.
pub const HILIGHTS_KIND: &str = "hilights";
/// Cache `params` of [`Job::ReadHilights`] results.
pub const HILIGHTS_PARAMS: &str = "v=1";
/// Cache `kind` of [`Job::Filmstrip`] thumbnails (one JPEG per entry).
pub const FILMSTRIP_KIND: &str = "filmstrip";
/// Version part of the cache `params` of [`Job::Filmstrip`] thumbnails; the
/// full params also name the count, height, number of chapters and index
/// (see [`filmstrip_params`]).
pub const FILMSTRIP_PARAMS: &str = "v=1";

/// Cache `params` of thumbnail `index` of a [`Job::Filmstrip`] over
/// `chapters` files.
pub fn filmstrip_params(chapters: usize, index: u32) -> String {
    format!(
        "{FILMSTRIP_PARAMS};n={FILMSTRIP_COUNT};h={FILMSTRIP_HEIGHT};chapters={chapters};i={index}"
    )
}

/// Identifies a submitted job. Unique within one [`JobRunner`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct JobId(pub u64);

/// Work for a background worker. Paths are absolute host paths.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Job {
    /// Find the music's beats: [`JobResult::MusicAnalysis`].
    AnalyzeMusic(PathBuf),
    /// Read container and stream metadata: [`JobResult::MediaInfo`].
    ProbeMedia(PathBuf),
    /// Read GoPro HiLight tags: [`JobResult::Hilights`]. A file that is not a
    /// GoPro MP4 gives an empty list, not an error.
    ReadHilights(PathBuf),
    /// Make the library thumbnails of a media item:
    /// [`JobResult::Filmstrip`]. `files` are the item's file, or a chaptered
    /// recording's chapters in order, with their durations (build it with
    /// [`Session::filmstrip_job`](crate::Session::filmstrip_job)).
    Filmstrip {
        media_id: MediaId,
        files: Vec<(PathBuf, Micros)>,
    },
}

impl Job {
    /// The file the job works on (a chaptered item's first chapter; empty
    /// for a filmstrip job without files).
    pub fn path(&self) -> &Path {
        match self {
            Job::AnalyzeMusic(p) | Job::ProbeMedia(p) | Job::ReadHilights(p) => p,
            Job::Filmstrip { files, .. } => files
                .first()
                .map(|(p, _)| p.as_path())
                .unwrap_or(Path::new("")),
        }
    }
}

/// What a finished job produced.
#[derive(Debug, Clone, PartialEq)]
pub enum JobResult {
    MusicAnalysis(MusicAnalysis),
    MediaInfo(MediaInfo),
    /// HiLight times, ascending.
    Hilights(Vec<Micros>),
    /// [`FILMSTRIP_COUNT`] JPEG thumbnails, in time order.
    Filmstrip(Vec<Vec<u8>>),
}

/// Progress of one job.
#[derive(Debug, Clone, PartialEq)]
pub enum JobEvent {
    /// A worker picked the job up.
    Started,
    /// Coarse progress, `0.0..=1.0`.
    Progress {
        fraction: f32,
    },
    Done {
        result: JobResult,
    },
    /// The job failed; `message` is a short, readable sentence.
    Failed {
        message: String,
    },
    /// The job was cancelled (or the runner shut down) before it finished.
    Cancelled,
}

impl JobEvent {
    /// Whether this is the job's last event.
    pub fn is_final(&self) -> bool {
        matches!(
            self,
            JobEvent::Done { .. } | JobEvent::Failed { .. } | JobEvent::Cancelled
        )
    }
}

/// One event on the runner's channel.
#[derive(Debug, Clone, PartialEq)]
pub struct JobMessage {
    pub id: JobId,
    pub job: Job,
    pub event: JobEvent,
}

/// The backends jobs use, injected so tests can use fakes.
#[derive(Clone)]
pub struct JobContext {
    pub media: Arc<dyn MediaBackend>,
    pub analyzer: Arc<dyn MusicAnalyzer>,
    pub cache: Arc<Cache>,
    /// Makes filmstrip thumbnails; usually
    /// [`MediaFilmstrips`](crate::MediaFilmstrips) over `media`.
    pub filmstrips: Arc<dyn FilmstripGenerator>,
}

impl JobContext {
    /// A context whose filmstrips are decoded with `media`.
    pub fn new(
        media: Arc<dyn MediaBackend>,
        analyzer: Arc<dyn MusicAnalyzer>,
        cache: Arc<Cache>,
    ) -> JobContext {
        JobContext {
            filmstrips: Arc::new(crate::MediaFilmstrips(Arc::clone(&media))),
            media,
            analyzer,
            cache,
        }
    }
}

impl fmt::Debug for JobContext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("JobContext").finish_non_exhaustive()
    }
}

/// Where a submitted job is, while it is not finished.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pending {
    Queued,
    Running,
    CancelRequested,
}

struct Shared {
    pending: Mutex<HashMap<JobId, Pending>>,
    shutdown: AtomicBool,
}

impl Shared {
    fn pending(&self) -> MutexGuard<'_, HashMap<JobId, Pending>> {
        // Workers never panic while holding the lock; recover anyway.
        self.pending.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// A fixed pool of worker threads running [`Job`]s.
///
/// Dropping the runner stops the workers: queued jobs are reported as
/// `Cancelled` and running ones finish in the background (their result is
/// still cached).
pub struct JobRunner {
    queue: Option<Sender<(JobId, Job)>>,
    shared: Arc<Shared>,
    next_id: AtomicU64,
}

impl fmt::Debug for JobRunner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("JobRunner").finish_non_exhaustive()
    }
}

impl JobRunner {
    /// Worker threads used by [`JobRunner::new`] (guess: enough to probe
    /// clips while a song is analysed, without starving the UI).
    pub const DEFAULT_WORKERS: usize = 2;

    /// A runner with [`JobRunner::DEFAULT_WORKERS`] workers, and the
    /// receiving end of its event channel.
    pub fn new(ctx: JobContext) -> (JobRunner, Receiver<JobMessage>) {
        Self::with_workers(ctx, Self::DEFAULT_WORKERS)
    }

    /// A runner with `workers` worker threads (at least one).
    pub fn with_workers(ctx: JobContext, workers: usize) -> (JobRunner, Receiver<JobMessage>) {
        let (queue_tx, queue_rx) = mpsc::channel::<(JobId, Job)>();
        let (events_tx, events_rx) = mpsc::channel();
        let queue_rx = Arc::new(Mutex::new(queue_rx));
        let shared = Arc::new(Shared {
            pending: Mutex::new(HashMap::new()),
            shutdown: AtomicBool::new(false),
        });
        for n in 0..workers.max(1) {
            let worker = Worker {
                ctx: ctx.clone(),
                queue: Arc::clone(&queue_rx),
                events: events_tx.clone(),
                shared: Arc::clone(&shared),
            };
            thread::Builder::new()
                .name(format!("kv-job-{n}"))
                .spawn(move || worker.run())
                .expect("failed to spawn a job worker thread");
        }
        let runner = JobRunner {
            queue: Some(queue_tx),
            shared,
            next_id: AtomicU64::new(1),
        };
        (runner, events_rx)
    }

    /// Queue a job. Its events arrive on the channel tagged with the
    /// returned id.
    pub fn submit(&self, job: Job) -> JobId {
        let id = JobId(self.next_id.fetch_add(1, Ordering::Relaxed));
        self.shared.pending().insert(id, Pending::Queued);
        tracing::debug!(?id, ?job, "job submitted");
        if let Some(queue) = &self.queue {
            // Workers only exit once the queue is closed (on drop), so this
            // cannot fail while `self` is alive.
            let _ = queue.send((id, job));
        }
        id
    }

    /// Cancel a job. A queued job never starts and reports `Cancelled`; a
    /// running job cannot be interrupted, but reports `Cancelled` instead
    /// of its result (which is still cached). Returns `false` if the job is
    /// unknown or already finished.
    pub fn cancel(&self, id: JobId) -> bool {
        match self.shared.pending().get_mut(&id) {
            Some(state) => {
                *state = Pending::CancelRequested;
                tracing::debug!(?id, "job cancel requested");
                true
            }
            None => false,
        }
    }
}

impl Drop for JobRunner {
    fn drop(&mut self) {
        self.shared.shutdown.store(true, Ordering::SeqCst);
        // Closing the queue lets idle workers exit.
        self.queue = None;
    }
}

struct Worker {
    ctx: JobContext,
    queue: Arc<Mutex<Receiver<(JobId, Job)>>>,
    events: Sender<JobMessage>,
    shared: Arc<Shared>,
}

impl Worker {
    fn run(self) {
        loop {
            let next = {
                let queue = self.queue.lock().unwrap_or_else(|e| e.into_inner());
                queue.recv()
            };
            let Ok((id, job)) = next else { break };
            self.run_one(id, job);
        }
    }

    fn send(&self, id: JobId, job: &Job, event: JobEvent) {
        // The receiver may be gone (app closing); results are still cached.
        let _ = self.events.send(JobMessage {
            id,
            job: job.clone(),
            event,
        });
    }

    fn run_one(&self, id: JobId, job: Job) {
        let start = {
            let mut pending = self.shared.pending();
            let cancelled = self.shared.shutdown.load(Ordering::SeqCst)
                || pending.get(&id) == Some(&Pending::CancelRequested);
            if cancelled {
                pending.remove(&id);
            } else {
                pending.insert(id, Pending::Running);
            }
            !cancelled
        };
        if !start {
            tracing::debug!(?id, "job cancelled before start");
            self.send(id, &job, JobEvent::Cancelled);
            return;
        }
        self.send(id, &job, JobEvent::Started);

        let mut progress = |fraction: f32| self.send(id, &job, JobEvent::Progress { fraction });
        let outcome = catch_unwind(AssertUnwindSafe(|| execute(&self.ctx, &job, &mut progress)))
            .unwrap_or_else(|_| {
                Err(format!(
                    "Something went wrong while reading \"{}\".",
                    file_label(job.path())
                ))
            });

        let cancelled = self.shared.pending().remove(&id) == Some(Pending::CancelRequested);
        let event = match outcome {
            _ if cancelled => JobEvent::Cancelled,
            Ok(result) => JobEvent::Done { result },
            Err(message) => {
                tracing::warn!(?id, %message, "job failed");
                JobEvent::Failed { message }
            }
        };
        self.send(id, &job, event);
    }
}

/// Run one job, cache first. The error is the user-facing message.
fn execute(
    ctx: &JobContext,
    job: &Job,
    progress: &mut dyn FnMut(f32),
) -> Result<JobResult, String> {
    let path = job.path();
    let name = file_label(path);
    match job {
        Job::AnalyzeMusic(_) => cached(
            &ctx.cache,
            path,
            MUSIC_ANALYSIS_KIND,
            MUSIC_ANALYSIS_PARAMS,
            progress,
            || {
                ctx.analyzer
                    .analyze(path)
                    .map_err(|e| format!("Couldn't analyse the music in \"{name}\": {e}"))
            },
        )
        .map(JobResult::MusicAnalysis),
        Job::ProbeMedia(_) => probe_cached(ctx, path, progress).map(JobResult::MediaInfo),
        Job::ReadHilights(_) => hilights_cached(ctx, path, progress).map(JobResult::Hilights),
        Job::Filmstrip { files, .. } => filmstrip_cached(ctx, files, progress)
            .map_err(|e| format!("Couldn't make thumbnails for \"{name}\": {e}"))
            .map(JobResult::Filmstrip),
    }
}

/// [`Job::ProbeMedia`]'s work, cache first.
pub(crate) fn probe_cached(
    ctx: &JobContext,
    path: &Path,
    progress: &mut dyn FnMut(f32),
) -> Result<MediaInfo, String> {
    let name = file_label(path);
    cached(&ctx.cache, path, PROBE_KIND, PROBE_PARAMS, progress, || {
        ctx.media
            .probe(path)
            .map_err(|e| format!("Couldn't read \"{name}\" as a video, photo or song: {e}"))
    })
}

/// [`Job::ReadHilights`]'s work, cache first.
pub(crate) fn hilights_cached(
    ctx: &JobContext,
    path: &Path,
    progress: &mut dyn FnMut(f32),
) -> Result<Vec<Micros>, String> {
    cached(
        &ctx.cache,
        path,
        HILIGHTS_KIND,
        HILIGHTS_PARAMS,
        progress,
        || read_hilights(path),
    )
}

/// [`Job::Filmstrip`]'s work: every thumbnail from the cache (keyed by the
/// first file), or all of them freshly generated and stored one JPEG per
/// cache entry.
fn filmstrip_cached(
    ctx: &JobContext,
    files: &[(PathBuf, Micros)],
    progress: &mut dyn FnMut(f32),
) -> Result<Vec<Vec<u8>>, String> {
    let (first, _) = files.first().ok_or("the item has no files")?;
    let key = SourceKey::from_file(first)
        .map_err(|e| format!("Couldn't read \"{}\": {}", file_label(first), io_detail(&e)))?;
    let params = |i| filmstrip_params(files.len(), i);
    let hits: Option<Vec<Vec<u8>>> = (0..FILMSTRIP_COUNT)
        .map(|i| match ctx.cache.get(&key, FILMSTRIP_KIND, &params(i)) {
            Ok(hit) => hit,
            Err(e) => {
                tracing::warn!(?first, error = %e, "cache read failed; regenerating thumbnails");
                None
            }
        })
        .collect();
    if let Some(thumbs) = hits {
        tracing::debug!(?first, "filmstrip cache hit");
        return Ok(thumbs);
    }
    progress(0.1);
    let thumbs = ctx
        .filmstrips
        .generate(files, FILMSTRIP_COUNT, FILMSTRIP_HEIGHT)
        .map_err(|e| e.to_string())?;
    if thumbs.len() != FILMSTRIP_COUNT as usize {
        return Err(format!(
            "expected {FILMSTRIP_COUNT} thumbnails, got {}",
            thumbs.len()
        ));
    }
    progress(0.9);
    for (i, jpeg) in (0..).zip(&thumbs) {
        if let Err(e) = ctx.cache.put(&key, FILMSTRIP_KIND, &params(i), jpeg) {
            tracing::warn!(?first, error = %e, "cache write failed");
            break;
        }
    }
    Ok(thumbs)
}

/// HiLights of `path`; a file that cannot be read is an error, a file that
/// is not a (GoPro) MP4 has none.
fn read_hilights(path: &Path) -> Result<Vec<Micros>, String> {
    match kv_gopro::read_hilights(path) {
        Ok(times) => Ok(times),
        Err(kv_gopro::GoproError::Io(e)) => Err(format!(
            "Couldn't read HiLights from \"{}\": {e}",
            file_label(path)
        )),
        Err(e) => {
            tracing::debug!(?path, error = %e, "no HiLights: not a readable GoPro MP4");
            Ok(Vec::new())
        }
    }
}

/// Look `path`'s result up in the cache, or compute and store it.
fn cached<T: Serialize + DeserializeOwned>(
    cache: &Cache,
    path: &Path,
    kind: &str,
    params: &str,
    progress: &mut dyn FnMut(f32),
    compute: impl FnOnce() -> Result<T, String>,
) -> Result<T, String> {
    // Stat before computing: if the file changes meanwhile, the result is
    // stored under the old identity and simply never hit.
    let key = SourceKey::from_file(path)
        .map_err(|e| format!("Couldn't read \"{}\": {}", file_label(path), io_detail(&e)))?;
    match cache.get_json::<T>(&key, kind, params) {
        Ok(Some(value)) => {
            tracing::debug!(?path, kind, "cache hit");
            return Ok(value);
        }
        Ok(None) => {}
        Err(e) => tracing::warn!(?path, kind, error = %e, "cache read failed; recomputing"),
    }
    progress(0.1);
    let value = compute()?;
    progress(0.9);
    if let Err(e) = cache.put_json(&key, kind, params, &value) {
        tracing::warn!(?path, kind, error = %e, "cache write failed");
    }
    Ok(value)
}

/// The I/O error inside a cache error, without the "i/o error:" prefix.
fn io_detail(e: &kv_cache::CacheError) -> String {
    match e {
        kv_cache::CacheError::Io(io) => io.to_string(),
        other => other.to_string(),
    }
}

/// The file name for messages, or the whole path if it has none.
fn file_label(path: &Path) -> String {
    path.file_name()
        .unwrap_or(path.as_os_str())
        .to_string_lossy()
        .into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Session;
    use kv_core::MusicRef;
    use kv_media::{DecodeOptions, MediaError, VideoReader};
    use std::error::Error;
    use std::sync::atomic::AtomicUsize;
    use std::time::Duration;

    const TIMEOUT: Duration = Duration::from_secs(20);

    fn analysis(tempo_bpm: f64) -> MusicAnalysis {
        MusicAnalysis {
            duration: Micros(4_000_000),
            tempo_bpm,
            beats: vec![Micros(0), Micros(500_000)],
            downbeats: vec![Micros(0)],
            onsets: vec![Micros(0)],
            energy: vec![0.5],
        }
    }

    /// Counts calls; optionally fails, or blocks until `gate` sends.
    #[derive(Default)]
    struct FakeAnalyzer {
        calls: AtomicUsize,
        fail: Option<String>,
        gate: Option<Mutex<Receiver<()>>>,
    }

    impl MusicAnalyzer for FakeAnalyzer {
        fn analyze(&self, _path: &Path) -> Result<MusicAnalysis, Box<dyn Error + Send + Sync>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if let Some(gate) = &self.gate {
                let _ = gate.lock().unwrap().recv();
            }
            match &self.fail {
                Some(msg) => Err(msg.clone().into()),
                None => Ok(analysis(120.0)),
            }
        }
    }

    #[derive(Default)]
    struct FakeMedia {
        calls: AtomicUsize,
    }

    impl MediaBackend for FakeMedia {
        fn probe(&self, path: &Path) -> kv_media::Result<MediaInfo> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if path.extension().is_some_and(|e| e == "bad") {
                return Err(MediaError::NoStreams(path.to_path_buf()));
            }
            Ok(MediaInfo {
                duration: Micros(1_000_000),
                container: "fake".into(),
                video: None,
                audio: None,
                creation_time: None,
            })
        }

        fn open_video(
            &self,
            path: &Path,
            _opts: &DecodeOptions,
        ) -> kv_media::Result<Box<dyn VideoReader>> {
            Err(MediaError::NoVideoStream(path.to_path_buf()))
        }
    }

    struct NoFilmstrips;

    impl FilmstripGenerator for NoFilmstrips {
        fn generate(
            &self,
            _files: &[(PathBuf, Micros)],
            _count: u32,
            _height: u32,
        ) -> Result<Vec<Vec<u8>>, Box<dyn Error + Send + Sync>> {
            Err("no filmstrips in this test".into())
        }
    }

    struct Fixture {
        dir: tempfile::TempDir,
        media: Arc<FakeMedia>,
        analyzer: Arc<FakeAnalyzer>,
        runner: JobRunner,
        events: Receiver<JobMessage>,
    }

    impl Fixture {
        fn new(analyzer: FakeAnalyzer, workers: usize) -> Fixture {
            let media = Arc::new(FakeMedia::default());
            let analyzer = Arc::new(analyzer);
            let ctx = JobContext {
                media: media.clone(),
                analyzer: analyzer.clone(),
                cache: Arc::new(Cache::in_memory().unwrap()),
                filmstrips: Arc::new(NoFilmstrips),
            };
            let (runner, events) = JobRunner::with_workers(ctx, workers);
            Fixture {
                dir: tempfile::tempdir().unwrap(),
                media,
                analyzer,
                runner,
                events,
            }
        }

        fn file(&self, name: &str, contents: &[u8]) -> PathBuf {
            let path = self.dir.path().join(name);
            std::fs::write(&path, contents).unwrap();
            path
        }

        /// All events of job `id` up to its final one (other jobs' events
        /// are dropped).
        fn wait(&self, id: JobId) -> Vec<JobEvent> {
            let mut seen = Vec::new();
            loop {
                let msg = self.events.recv_timeout(TIMEOUT).expect("job event");
                if msg.id == id {
                    let done = msg.event.is_final();
                    seen.push(msg.event);
                    if done {
                        return seen;
                    }
                }
            }
        }

        fn run(&self, job: Job) -> JobEvent {
            let id = self.runner.submit(job);
            self.wait(id).pop().unwrap()
        }
    }

    #[test]
    fn analyze_music_reports_started_progress_and_done() {
        let fx = Fixture::new(FakeAnalyzer::default(), 2);
        let song = fx.file("song.wav", b"abc");
        let id = fx.runner.submit(Job::AnalyzeMusic(song));
        let events = fx.wait(id);
        assert_eq!(events.first(), Some(&JobEvent::Started));
        assert!(events.iter().any(
            |e| matches!(e, JobEvent::Progress { fraction } if (0.0..=1.0).contains(fraction))
        ));
        assert_eq!(
            events.last(),
            Some(&JobEvent::Done {
                result: JobResult::MusicAnalysis(analysis(120.0))
            })
        );
    }

    #[test]
    fn cache_hit_does_not_call_the_analyzer_again() {
        let fx = Fixture::new(FakeAnalyzer::default(), 2);
        let song = fx.file("song.wav", b"abc");
        let first = fx.run(Job::AnalyzeMusic(song.clone()));
        let second = fx.run(Job::AnalyzeMusic(song));
        assert!(matches!(first, JobEvent::Done { .. }));
        assert_eq!(first, second);
        assert_eq!(fx.analyzer.calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn changed_file_size_reruns_the_analyzer() {
        let fx = Fixture::new(FakeAnalyzer::default(), 2);
        let song = fx.file("song.wav", b"abc");
        fx.run(Job::AnalyzeMusic(song.clone()));
        fx.file("song.wav", b"abcdef");
        fx.run(Job::AnalyzeMusic(song));
        assert_eq!(fx.analyzer.calls.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn probe_is_cached_too() {
        let fx = Fixture::new(FakeAnalyzer::default(), 2);
        let clip = fx.file("clip.mp4", b"abc");
        let first = fx.run(Job::ProbeMedia(clip.clone()));
        let second = fx.run(Job::ProbeMedia(clip));
        assert!(matches!(
            first,
            JobEvent::Done {
                result: JobResult::MediaInfo(_)
            }
        ));
        assert_eq!(first, second);
        assert_eq!(fx.media.calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn failure_event_carries_a_readable_message() {
        let fx = Fixture::new(
            FakeAnalyzer {
                fail: Some("unsupported audio: opus".into()),
                ..Default::default()
            },
            2,
        );
        let song = fx.file("song.ogg", b"abc");
        let JobEvent::Failed { message } = fx.run(Job::AnalyzeMusic(song)) else {
            panic!("expected Failed");
        };
        assert_eq!(
            message,
            "Couldn't analyse the music in \"song.ogg\": unsupported audio: opus"
        );

        let clip = fx.file("clip.bad", b"abc");
        let JobEvent::Failed { message } = fx.run(Job::ProbeMedia(clip)) else {
            panic!("expected Failed");
        };
        assert!(
            message.starts_with("Couldn't read \"clip.bad\""),
            "{message}"
        );
    }

    #[test]
    fn missing_file_fails_with_its_name() {
        let fx = Fixture::new(FakeAnalyzer::default(), 2);
        let missing = fx.dir.path().join("gone.wav");
        let JobEvent::Failed { message } = fx.run(Job::AnalyzeMusic(missing)) else {
            panic!("expected Failed");
        };
        assert!(
            message.starts_with("Couldn't read \"gone.wav\": "),
            "{message}"
        );
        assert!(!message.contains("i/o error"), "{message}");
        assert_eq!(fx.analyzer.calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn non_gopro_file_has_no_hilights() {
        let fx = Fixture::new(FakeAnalyzer::default(), 2);
        let not_mp4 = fx.file("photo.jpg", b"\xff\xd8\xff\xe0 not an mp4 at all");
        assert_eq!(
            fx.run(Job::ReadHilights(not_mp4)),
            JobEvent::Done {
                result: JobResult::Hilights(Vec::new())
            }
        );
        let missing = fx.dir.path().join("gone.mp4");
        assert!(matches!(
            fx.run(Job::ReadHilights(missing)),
            JobEvent::Failed { .. }
        ));
    }

    #[test]
    fn cancel_before_start_yields_only_cancelled() {
        let (gate_tx, gate_rx) = mpsc::channel();
        let fx = Fixture::new(
            FakeAnalyzer {
                gate: Some(Mutex::new(gate_rx)),
                ..Default::default()
            },
            1,
        );
        let song = fx.file("song.wav", b"abc");
        // Occupy the only worker.
        let busy = fx.runner.submit(Job::AnalyzeMusic(song.clone()));
        let started = fx.events.recv_timeout(TIMEOUT).unwrap();
        assert_eq!((started.id, started.event), (busy, JobEvent::Started));

        let queued = fx.runner.submit(Job::ProbeMedia(song));
        assert!(fx.runner.cancel(queued));
        gate_tx.send(()).unwrap();

        assert!(matches!(fx.wait(busy).last(), Some(JobEvent::Done { .. })));
        assert_eq!(fx.wait(queued), vec![JobEvent::Cancelled]);
        assert_eq!(fx.media.calls.load(Ordering::SeqCst), 0);
        // Finished jobs cannot be cancelled.
        assert!(!fx.runner.cancel(queued));
        assert!(!fx.runner.cancel(busy));
    }

    #[test]
    fn cancel_while_running_reports_cancelled_but_caches() {
        let (gate_tx, gate_rx) = mpsc::channel();
        let fx = Fixture::new(
            FakeAnalyzer {
                gate: Some(Mutex::new(gate_rx)),
                ..Default::default()
            },
            1,
        );
        let song = fx.file("song.wav", b"abc");
        let id = fx.runner.submit(Job::AnalyzeMusic(song.clone()));
        assert_eq!(
            fx.events.recv_timeout(TIMEOUT).unwrap().event,
            JobEvent::Started
        );
        assert!(fx.runner.cancel(id));
        gate_tx.send(()).unwrap();
        assert_eq!(fx.wait(id).last(), Some(&JobEvent::Cancelled));

        // The result was cached anyway: no second analyzer call.
        assert!(matches!(
            fx.run(Job::AnalyzeMusic(song)),
            JobEvent::Done { .. }
        ));
        assert_eq!(fx.analyzer.calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn jobs_run_in_parallel_on_default_workers() {
        let (gate_tx, gate_rx) = mpsc::channel();
        let media = Arc::new(FakeMedia::default());
        let ctx = JobContext {
            media: media.clone(),
            analyzer: Arc::new(FakeAnalyzer {
                gate: Some(Mutex::new(gate_rx)),
                ..Default::default()
            }),
            cache: Arc::new(Cache::in_memory().unwrap()),
            filmstrips: Arc::new(NoFilmstrips),
        };
        let (runner, events) = JobRunner::new(ctx);
        let dir = tempfile::tempdir().unwrap();
        let song = dir.path().join("song.wav");
        std::fs::write(&song, b"abc").unwrap();
        // The analysis blocks one worker; the probe still finishes.
        let a = runner.submit(Job::AnalyzeMusic(song.clone()));
        let p = runner.submit(Job::ProbeMedia(song));
        loop {
            let msg = events.recv_timeout(TIMEOUT).unwrap();
            if msg.id == p && msg.event.is_final() {
                assert!(matches!(msg.event, JobEvent::Done { .. }));
                break;
            }
            assert!(!(msg.id == a && msg.event.is_final()));
        }
        gate_tx.send(()).unwrap();
    }

    #[test]
    fn panicking_job_fails_and_the_worker_survives() {
        struct Panics;
        impl MusicAnalyzer for Panics {
            fn analyze(&self, _: &Path) -> Result<MusicAnalysis, Box<dyn Error + Send + Sync>> {
                panic!("boom");
            }
        }
        let media = Arc::new(FakeMedia::default());
        let ctx = JobContext {
            media,
            analyzer: Arc::new(Panics),
            cache: Arc::new(Cache::in_memory().unwrap()),
            filmstrips: Arc::new(NoFilmstrips),
        };
        let (runner, events) = JobRunner::with_workers(ctx, 1);
        let dir = tempfile::tempdir().unwrap();
        let song = dir.path().join("song.wav");
        std::fs::write(&song, b"abc").unwrap();
        let a = runner.submit(Job::AnalyzeMusic(song.clone()));
        let b = runner.submit(Job::ProbeMedia(song));
        let finals: Vec<_> = events
            .iter()
            .filter(|m| m.event.is_final())
            .take(2)
            .collect();
        assert_eq!(finals[0].id, a);
        assert!(matches!(finals[0].event, JobEvent::Failed { .. }));
        assert_eq!(finals[1].id, b);
        assert!(matches!(finals[1].event, JobEvent::Done { .. }));
    }

    #[test]
    fn session_keeps_finished_music_analysis() {
        let fx = Fixture::new(FakeAnalyzer::default(), 2);
        let song = fx.file("song.wav", b"abc");
        let mut session = Session::new(fx.dir.path().to_path_buf());
        let music = MusicRef {
            path: "song.wav".into(),
            duration: Micros(4_000_000),
        };
        assert_eq!(session.music_analysis(&music), None);

        let id = fx.runner.submit(Job::AnalyzeMusic(song.clone()));
        let mut stored = false;
        while !stored {
            let msg = fx.events.recv_timeout(TIMEOUT).unwrap();
            assert_eq!(msg.id, id);
            stored = session.handle_job_message(&msg);
            assert!(!msg.event.is_final() || stored);
        }
        assert_eq!(session.music_analysis(&music), Some(&analysis(120.0)));

        // Other jobs' results are ignored.
        let probe = fx.runner.submit(Job::ProbeMedia(song));
        for msg in fx.wait_msgs(probe) {
            assert!(!session.handle_job_message(&msg));
        }
    }

    impl Fixture {
        fn wait_msgs(&self, id: JobId) -> Vec<JobMessage> {
            let mut seen = Vec::new();
            loop {
                let msg = self.events.recv_timeout(TIMEOUT).expect("job event");
                let done = msg.id == id && msg.event.is_final();
                seen.push(msg);
                if done {
                    return seen;
                }
            }
        }
    }
}
