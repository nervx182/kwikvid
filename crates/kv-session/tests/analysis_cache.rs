//! Real `kv_audio::analyze` through the job runner, with an on-disk cache
//! that survives being reopened.

use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use kv_audio::MusicAnalysis;
use kv_cache::{Cache, SourceKey};
use kv_media::{DecodeOptions, MediaBackend, MediaError, MediaInfo, VideoReader};
use kv_session::{
    AudioAnalyzer, Job, JobContext, JobEvent, JobResult, JobRunner, MusicAnalyzer,
    MUSIC_ANALYSIS_KIND, MUSIC_ANALYSIS_PARAMS,
};

/// The test never probes; there is no FFmpeg here.
struct NoMedia;

impl MediaBackend for NoMedia {
    fn probe(&self, path: &Path) -> kv_media::Result<MediaInfo> {
        Err(MediaError::NoStreams(path.to_path_buf()))
    }
    fn open_video(&self, path: &Path, _: &DecodeOptions) -> kv_media::Result<Box<dyn VideoReader>> {
        Err(MediaError::NoVideoStream(path.to_path_buf()))
    }
}

/// Wraps the real analyzer and counts calls.
struct Counting(AtomicUsize);

impl MusicAnalyzer for Counting {
    fn analyze(
        &self,
        path: &Path,
    ) -> Result<MusicAnalysis, Box<dyn std::error::Error + Send + Sync>> {
        self.0.fetch_add(1, Ordering::SeqCst);
        AudioAnalyzer.analyze(path)
    }
}

fn analyze_once(cache_file: &Path, song: &Path, analyzer: Arc<Counting>) -> MusicAnalysis {
    let ctx = JobContext::new(
        Arc::new(NoMedia),
        analyzer,
        Arc::new(Cache::open(cache_file).unwrap()),
    );
    let (runner, events) = JobRunner::new(ctx);
    let id = runner.submit(Job::AnalyzeMusic(song.to_path_buf()));
    loop {
        let msg = events.recv_timeout(Duration::from_secs(120)).unwrap();
        assert_eq!(msg.id, id);
        match msg.event {
            JobEvent::Done {
                result: JobResult::MusicAnalysis(a),
            } => return a,
            e if e.is_final() => panic!("unexpected {e:?}"),
            _ => {}
        }
    }
}

#[test]
fn real_analysis_is_cached_across_reopen() {
    let song = kv_testkit::click_track(120.0, 6.0, 22_050);
    let dir = tempfile::tempdir().unwrap();
    let cache_file = dir.path().join("cache.sqlite");

    let analyzer = Arc::new(Counting(AtomicUsize::new(0)));
    let first = analyze_once(&cache_file, &song, analyzer.clone());
    assert!(
        (first.tempo_bpm - 120.0).abs() < 2.0,
        "tempo {}",
        first.tempo_bpm
    );
    assert!(!first.beats.is_empty());

    // Reopen the cache file directly: the result is there.
    let cache = Cache::open(&cache_file).unwrap();
    let key = SourceKey::from_file(&song).unwrap();
    let stored: MusicAnalysis = cache
        .get_json(&key, MUSIC_ANALYSIS_KIND, MUSIC_ANALYSIS_PARAMS)
        .unwrap()
        .expect("analysis cached");
    assert_eq!(stored, first);
    drop(cache);

    // A new runner on the reopened cache is served without analysing.
    let second = analyze_once(&cache_file, &song, analyzer.clone());
    assert_eq!(second, first);
    assert_eq!(analyzer.0.load(Ordering::SeqCst), 1);
}
