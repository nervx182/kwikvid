//! Live preview playback rate and seek latency at 1080p (issue #148),
//! measured against the Roadmap I targets "real-time 1080p preview at 30 fps"
//! and "seek < 150 ms" (`docs/roadmap.md`, MVP item 10).
//!
//! Each case drives a [`PreviewEngine`] end to end, as the app does: the
//! `FfmpegBackend` decoder, a 1920x1080 compositor, a counting [`FrameSink`],
//! [`Pacer`] ticking it on a thread and the system clock, 30 fps, no audio.
//! The compositor is [`kv_render::gpu::GpuCompositor`] when a hardware GPU
//! adapter is available (its sink waits for the GPU to finish each frame,
//! like presenting it would), else [`CpuCompositor`]; the bench prints which
//! one ran and the adapter.
//!
//! The edit is hand-built: about 24 s of segments 1 to 2 s long (cuts) cycling
//! over 12 media items, with one 0.5 s crossfade. Source cases:
//!
//! - `h264-1080p30`: 12 tagged 1080p30 H.264 clips of 6 s from `kv_testkit`;
//! - `hevc10-2160p60`: the 6 s 2160p60 HEVC Main10 clip of the `export` bench
//!   (`benches/common/hevc10.rs`), all 12 media items pointing at it; skipped
//!   with a `SKIPPED:` line when FFmpeg has no 10-bit HEVC encoder;
//! - every checked-out regression-corpus clip (`fixtures/corpus.toml`) the
//!   same way; missing ones are skipped by `corpus_file_or_skip!`.
//!
//! # Measurements
//!
//! - **Playback.** From the first frame on screen, play [`PLAY`] (10 s) and
//!   count the frames the engine presented ([`PreviewStats`]). Frames
//!   missed are the frames due in the wall time played (`wall x 30`, less
//!   one for the edges) that were not presented, or the engine's dropped
//!   count if larger (a frame still waiting for its decode is not counted as
//!   dropped yet). The reported value is the mean time per presented frame
//!   on the edit timeline: `33.3 ms x (presented + missed) / presented`, so
//!   33.3 ms means every frame was shown on time (30 fps) and each missed
//!   frame raises it. Printed as
//!   `TARGET preview-fps-1080p-<case> <ms per presented frame> 33.3`, plus a
//!   `time:`-style line with the achieved fps and the dropped frame count.
//! - **Seek.** While paused, [`SEEKS`] (20) seeks to pseudo-random positions;
//!   each waits for its frame to reach the sink and takes the latency from
//!   [`PreviewStats::last_seek_latency`] (seek call to frame presented, the
//!   number the app's transport reports). Printed as
//!   `TARGET preview-seek-p50-<case> <ms> 150` and
//!   `TARGET preview-seek-p95-<case> <ms> 150`, plus a `time:`-style line
//!   (`[p50 p95 max]`).
//!
//! `TARGET` lines follow AGENTS.md ("Full regression suite"); the regression
//! `benchmarks` job turns them into its "Performance targets" table. The CI
//! Mac has a paravirtual GPU (ADR 0007), so its numbers are indicative; #21
//! is the check on real hardware.
//!
//! A plain timed run, not criterion (a 10 s playback is one sample).
//! Runs in the regression `benchmarks` job and on Linux (CPU compositor):
//! `cargo bench -p kv-render --bench preview`. Without `--bench` (for example
//! `cargo test --benches`) it exits at once.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use kv_core::{
    Edl, MediaId, MediaItem, MediaKind, Micros, Project, ProjectId, Segment, Speed, Transition,
};
use kv_media::{FfmpegBackend, MediaBackend, Rational};
use kv_render::preview::{
    CpuCompositor, FrameSink, Pacer, PreviewCompositor, PreviewConfig, PreviewEngine, PreviewStats,
    RenderedFrame, SystemClock,
};
use kv_testkit::{tagged_clip, ClipSpec};

#[path = "common/hevc10.rs"]
mod hevc10;

/// Preview (compositor) size.
const SIZE: (u32, u32) = (1920, 1080);
const FPS: i64 = 30;
/// The Roadmap I playback budget: one frame interval at 30 fps.
const FRAME_TARGET_MS: f64 = 1000.0 / FPS as f64;
/// The Roadmap I seek budget.
const SEEK_TARGET: Duration = Duration::from_millis(150);
/// Playback measured per case.
const PLAY: Duration = Duration::from_secs(10);
/// Seeks measured per case.
const SEEKS: usize = 20;
/// Longest wait for one frame (first frame, a seek) before giving up.
const FRAME_TIMEOUT: Duration = Duration::from_secs(10);
/// Media items per edit.
const ITEMS: usize = 12;
/// Length of each generated H.264 clip.
const CLIP_SECONDS: f64 = 6.0;
/// The edit is at least this long.
const EDIT_SECONDS: f64 = 24.0;
/// Segment lengths, cycled: a cut every 1 to 2 s.
const SEGMENT_SECONDS: [f64; 5] = [1.0, 1.5, 2.0, 1.25, 1.75];
/// The segment that starts with the crossfade, and its length.
const CROSSFADE_AT: usize = 6;
const CROSSFADE: f64 = 0.5;

fn us(seconds: f64) -> Micros {
    Micros((seconds * 1e6).round() as i64)
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1e3
}

/// An absolute path as a project path (the engine resolves paths starting
/// with `/` as absolute).
fn project_path(path: &Path) -> String {
    let abs = std::fs::canonicalize(path).expect("fixture exists");
    abs.to_string_lossy().replace('\\', "/")
}

/// The benchmark edit over `sources` (path, duration), one media item per
/// source; `None` when a source is too short for the longest segment.
fn project(sources: &[(PathBuf, Micros)]) -> Option<Project> {
    let mut p = Project::new(ProjectId(1));
    for (i, (path, duration)) in sources.iter().enumerate() {
        p.media.push(MediaItem {
            id: MediaId(i as u64 + 1),
            path: project_path(path),
            duration: *duration,
            kind: MediaKind::Video,
            hilights: vec![],
            clip_audio: false,
            chapters: Vec::new(),
        });
    }
    let longest = SEGMENT_SECONDS.iter().copied().fold(0.0, f64::max) + CROSSFADE;
    if sources.iter().any(|s| s.1.as_secs_f64() < longest + 0.1) {
        return None;
    }
    let mut edl = Edl { segments: vec![] };
    let mut total = 0.0;
    let mut k = 0usize;
    while total < EDIT_SECONDS {
        let (id, duration) = (k % sources.len(), sources[k % sources.len()].1);
        let fade = k == CROSSFADE_AT;
        let len = SEGMENT_SECONDS[k % SEGMENT_SECONDS.len()];
        let seg_len = if fade { len + CROSSFADE } else { len };
        // Spread the in points over the source.
        let room = duration.as_secs_f64() - seg_len;
        let src_in = (k as f64 * 0.73) % room.max(0.01);
        edl.segments.push(Segment {
            media_id: MediaId(id as u64 + 1),
            src_in: us(src_in),
            src_out: us(src_in + seg_len),
            timeline_start: Micros(0),
            transition: if fade {
                Transition::Crossfade {
                    duration: us(CROSSFADE),
                }
            } else {
                Transition::Cut
            },
            speed: Speed::NORMAL,
        });
        total += len;
        k += 1;
    }
    edl.repack();
    p.sections[0].edit = edl;
    Some(p)
}

/// Counts presented frames; with a GPU compositor, also waits for the GPU
/// to finish each frame (presenting it to a window would).
struct CountingSink {
    presented: Arc<AtomicU64>,
    #[cfg(feature = "gpu")]
    device: Option<wgpu::Device>,
}

impl FrameSink for CountingSink {
    fn present(&mut self, frame: &RenderedFrame<'_>) {
        #[cfg(feature = "gpu")]
        if let Some(device) = &self.device {
            let _ = device.poll(wgpu::PollType::Wait);
        }
        std::hint::black_box(frame.pts());
        self.presented.fetch_add(1, Ordering::SeqCst);
    }
}

/// The compositor to use: a GPU one on a hardware adapter, else the CPU
/// one. Returns it, the device the sink waits on, and a description.
#[cfg(feature = "gpu")]
fn compositor() -> (Box<dyn PreviewCompositor>, Option<wgpu::Device>, String) {
    use kv_render::gpu::{GpuCompositor, GpuContext, WgpuContext};
    let reason = match WgpuContext::headless() {
        Ok(ctx) => {
            let info = ctx.adapter_info();
            let name = format!("{} ({:?}, {:?})", info.name, info.backend, info.device_type);
            if info.device_type == wgpu::DeviceType::Cpu {
                format!("software adapter {name}")
            } else {
                match GpuCompositor::new(&ctx, SIZE.0, SIZE.1) {
                    Ok(c) => {
                        return (
                            Box::new(c),
                            Some(ctx.device().clone()),
                            format!("gpu on {name}"),
                        )
                    }
                    Err(e) => format!("gpu compositor failed on {name}: {e}"),
                }
            }
        }
        // The full error lists every backend; the first clause says enough.
        Err(e) => e
            .to_string()
            .split(';')
            .next()
            .unwrap_or_default()
            .to_string(),
    };
    let c = CpuCompositor::new(SIZE.0, SIZE.1).expect("valid size");
    (
        Box::new(c),
        None,
        format!("cpu (no hardware GPU: {reason})"),
    )
}

#[cfg(not(feature = "gpu"))]
fn compositor() -> (Box<dyn PreviewCompositor>, Option<()>, String) {
    let c = CpuCompositor::new(SIZE.0, SIZE.1).expect("valid size");
    (
        Box::new(c),
        None,
        "cpu (built without the gpu feature)".into(),
    )
}

/// Wait until the sink has seen more than `seen` frames.
fn wait_presented(count: &AtomicU64, seen: u64) -> bool {
    let started = Instant::now();
    while count.load(Ordering::SeqCst) <= seen {
        if started.elapsed() > FRAME_TIMEOUT {
            return false;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    true
}

/// p-th percentile (0..=100) of sorted values, nearest rank.
fn percentile(sorted: &[Duration], p: f64) -> Duration {
    let rank = ((p / 100.0) * sorted.len() as f64).ceil() as usize;
    sorted[rank.clamp(1, sorted.len()) - 1]
}

fn bench_case(media: &Arc<dyn MediaBackend>, label: &str, project: &Project) {
    let name = label.replace('/', "-");
    let (compositor, device, which) = compositor();
    println!("preview/{label}: compositor {which}");
    let presented = Arc::new(AtomicU64::new(0));
    let sink = CountingSink {
        presented: Arc::clone(&presented),
        #[cfg(feature = "gpu")]
        device,
    };
    #[cfg(not(feature = "gpu"))]
    let _ = device;
    let config = PreviewConfig {
        fps: Rational::new(FPS as i32, 1),
        media_root: PathBuf::from("/"),
        clock: Arc::new(SystemClock::new()),
        ..PreviewConfig::default()
    };
    let engine = match PreviewEngine::new(Arc::clone(media), compositor, Box::new(sink), config) {
        Ok(e) => Arc::new(e),
        Err(e) => {
            eprintln!("SKIPPED: preview/{label}: {e}");
            return;
        }
    };
    if let Err(e) = engine.set_edit(project, Project::DEFAULT_SECTION) {
        eprintln!("SKIPPED: preview/{label}: {e}");
        return;
    }
    let _pacer = Pacer::spawn(Arc::clone(&engine));

    // Playback, from the first frame on screen.
    if !wait_presented(&presented, 0) {
        eprintln!("SKIPPED: preview/{label}: no first frame within {FRAME_TIMEOUT:?}");
        return;
    }
    let before = engine.stats();
    let started = Instant::now();
    engine.play();
    std::thread::sleep(PLAY);
    engine.pause();
    let wall = started.elapsed();
    let after = engine.stats();
    let shown = after.presented - before.presented;
    let dropped = after.dropped - before.dropped;
    let due = (wall.as_secs_f64() * FPS as f64).round() as u64;
    let missed = dropped.max(due.saturating_sub(shown + 1));
    // No frame at all counts as one, so the value stays finite for the
    // summary table.
    let per_frame_ms = FRAME_TARGET_MS * (shown + missed) as f64 / shown.max(1) as f64;
    let fps = shown as f64 / wall.as_secs_f64();
    println!(
        "preview/{label}/play {:.1} s at {}x{}: {shown} presented, {missed} missed ({dropped} dropped)",
        wall.as_secs_f64(),
        SIZE.0,
        SIZE.1
    );
    println!(
        "                        time:   [{per_frame_ms:.1} ms per frame]  {fps:.1} fps, {missed} missed, target {FRAME_TARGET_MS:.1} ms: {}",
        if per_frame_ms <= FRAME_TARGET_MS + 0.05 { "met" } else { "MISSED" }
    );
    println!("TARGET preview-fps-1080p-{name} {per_frame_ms:.1} {FRAME_TARGET_MS:.1}");

    // Seeks while paused.
    let end = project.sections[0]
        .edit
        .segments
        .last()
        .map_or(1, |s| (s.timeline_start.0 + s.timeline_len().0).max(1)) as u64;
    let mut seed = 0x2545_F491_4F6C_DD1D_u64;
    let mut latencies = Vec::with_capacity(SEEKS);
    for _ in 0..SEEKS {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        let t = Micros((seed % end) as i64);
        let seen = presented.load(Ordering::SeqCst);
        engine.seek(t);
        if !wait_presented(&presented, seen) {
            eprintln!(
                "preview/{label}: seek to {:.2} s: no frame within {FRAME_TIMEOUT:?}",
                t.as_secs_f64()
            );
            latencies.push(FRAME_TIMEOUT);
            continue;
        }
        let stats: PreviewStats = engine.stats();
        latencies.push(stats.last_seek_latency.unwrap_or(FRAME_TIMEOUT));
    }
    latencies.sort_unstable();
    let (p50, p95, max) = (
        percentile(&latencies, 50.0),
        percentile(&latencies, 95.0),
        latencies[latencies.len() - 1],
    );
    println!("preview/{label}/seek p50/p95/max over {SEEKS} seeks while paused");
    println!(
        "                        time:   [{:.1} ms {:.1} ms {:.1} ms]  target {:.0} ms: p95 {}",
        ms(p50),
        ms(p95),
        ms(max),
        ms(SEEK_TARGET),
        if p95 <= SEEK_TARGET { "met" } else { "MISSED" }
    );
    println!(
        "TARGET preview-seek-p50-{name} {:.1} {:.0}",
        ms(p50),
        ms(SEEK_TARGET)
    );
    println!(
        "TARGET preview-seek-p95-{name} {:.1} {:.0}",
        ms(p95),
        ms(SEEK_TARGET)
    );
}

/// Probe `path` and build the edit with `ITEMS` media items pointing at it.
fn single_source_project(
    media: &Arc<dyn MediaBackend>,
    label: &str,
    path: &Path,
) -> Option<Project> {
    let info = match media.probe(path) {
        Ok(info) => info,
        Err(e) => {
            eprintln!(
                "SKIPPED: preview/{label}: cannot probe {}: {e}",
                path.display()
            );
            return None;
        }
    };
    let Some(v) = &info.video else {
        eprintln!("SKIPPED: preview/{label}: no video stream");
        return None;
    };
    eprintln!(
        "preview/{label}: source {} {}x{} {}-bit, {:.1} s",
        v.codec,
        v.width,
        v.height,
        v.bit_depth,
        info.duration.as_secs_f64()
    );
    let p = project(&vec![(path.to_path_buf(), info.duration); ITEMS]);
    if p.is_none() {
        eprintln!("SKIPPED: preview/{label}: source too short");
    }
    p
}

fn bench_corpus_file(media: &Arc<dyn MediaBackend>, name: &str) {
    let path = kv_testkit::corpus_file_or_skip!(name);
    let label = format!("corpus/{name}");
    if let Some(p) = single_source_project(media, &label, &path) {
        bench_case(media, &label, &p);
    }
}

fn main() {
    if !std::env::args().any(|a| a == "--bench") {
        eprintln!("preview bench: run with `cargo bench -p kv-render --bench preview`");
        return;
    }
    let media: Arc<dyn MediaBackend> = Arc::new(FfmpegBackend::new().expect("FFmpeg initialises"));
    if let Ok(v) = std::env::var("KV_HWDECODE") {
        println!("preview: KV_HWDECODE={v}");
    }

    // (a) Tagged 1080p30 H.264 clips.
    let sources: Vec<(PathBuf, Micros)> = (0..ITEMS)
        .map(|i| {
            let path = tagged_clip(&ClipSpec {
                clip: i as u8,
                seconds: CLIP_SECONDS,
                width: 1920,
                height: 1080,
                ..ClipSpec::default()
            });
            (path, us(CLIP_SECONDS))
        })
        .collect();
    let p = project(&sources).expect("6 s clips are long enough");
    bench_case(&media, "h264-1080p30", &p);

    // (b) 2160p60 HEVC Main10, shared with the export bench.
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("kv-render-export-bench");
    std::fs::create_dir_all(&dir).expect("create bench dir");
    match hevc10::hevc10_clip(&dir) {
        Ok(path) => {
            if let Some(p) = single_source_project(&media, "hevc10-2160p60", &path) {
                bench_case(&media, "hevc10-2160p60", &p);
            }
        }
        Err(e) => eprintln!("SKIPPED: preview/hevc10-2160p60: {e}"),
    }

    for entry in &kv_testkit::corpus_manifest().files {
        bench_corpus_file(&media, &entry.name);
    }
}
