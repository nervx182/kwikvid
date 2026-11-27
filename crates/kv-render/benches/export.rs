//! 60 s 1080p export time (issue #147), measured against the Roadmap I target
//! "60 s 1080p export < 20 s" (`docs/roadmap.md`).
//!
//! Each case exports a 60 s edit at 1920x1080, 30 fps, H.264 with
//! [`kv_render::export`], `FfmpegBackend` as both media backend and encoder,
//! and a 128 BPM click track as the music bed. The edit is hand-built: 12 or
//! more segments, one per media item (cuts, plus a 0.5 s crossfade on every
//! fourth segment), filling exactly 60 s. Source cases:
//!
//! - `h264-1080p30`: 12 tagged 1080p30 H.264 clips of 6 s from `kv_testkit`
//!   (`tagged_clip`, cached in `kv_testkit::fixtures_dir`);
//! - `hevc10-2160p60`: one 6 s 2160p60 HEVC Main10 clip,
//!   [`kv_testkit::HEVC10_2160P60`] from [`kv_testkit::synthetic_clip`]
//!   (`libx265` or `hevc_videotoolbox`; skipped with a `SKIPPED:` line when
//!   the FFmpeg build has neither), cached in `kv_testkit::fixtures_dir` and
//!   shared with `crates/kv-media/benches/seek.rs`. The 12 media items all
//!   point at that file, so every segment opens its own reader and decodes
//!   like a separate clip;
//! - every checked-out regression-corpus clip (`fixtures/corpus.toml`, ADR
//!   0008) the same way: the media items all point at the clip, which is
//!   skipped by `corpus_file_or_skip!` when not checked out.
//!
//! # Timing
//!
//! One export takes seconds, so this is a plain timed loop instead of
//! criterion (whose minimum of 10 samples would take minutes per case): each
//! case runs [`RUNS`] exports and reports the minimum and the median (of the
//! runs done: a case stops early once its runs took [`CASE_BUDGET`]), which
//! keeps the whole bench under about 10 minutes on the Mac runner. The first
//! run includes opening the decoders cold, like a user's export.
//!
//! # Output
//!
//! Per case, a line in criterion's `time:` style (`[min median max]`) for the
//! raw block of the regression job summary, the encoder used
//! (`ExportReport::encoder_name`, `h264_videotoolbox` on the Mac runner), and
//! one machine-readable line
//!
//! ```text
//! TARGET <name> <measured_ms> <target_ms>
//! ```
//!
//! with the median as `<measured_ms>`. The regression `benchmarks` job turns
//! these into its "Performance targets" table (AGENTS.md, "Full regression
//! suite").
//!
//! Runs only in the regression `benchmarks` job:
//! `cargo bench -p kv-render --bench export`. Without `--bench` (for example
//! `cargo test --benches`) it exits at once.

use std::ops::ControlFlow;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use kv_core::{
    Edl, MediaId, MediaItem, MediaKind, Micros, MusicRef, Project, ProjectId, Segment, Speed,
    Transition,
};
use kv_media::{ExportSettings, FfmpegBackend, MediaBackend, Rational};
use kv_render::{export, ExportReport};
use kv_testkit::{click_track, synthetic_clip, tagged_clip, ClipSpec, HEVC10_2160P60};

/// The Roadmap I export budget.
const TARGET: Duration = Duration::from_secs(20);
/// Exports per case.
const RUNS: usize = 3;
/// No further run of a case starts once its runs took this long in total,
/// so a very slow case (4K HEVC on a CPU-only decode) cannot push the bench
/// past ~10 minutes.
const CASE_BUDGET: Duration = Duration::from_secs(180);
/// Length of the exported edit.
const EDIT_SECONDS: f64 = 60.0;
/// Media items (and segments, at least) per edit.
const ITEMS: usize = 12;
/// Length of each generated source clip.
const CLIP_SECONDS: f64 = 6.0;
const CROSSFADE: f64 = 0.5;

fn us(seconds: f64) -> Micros {
    Micros((seconds * 1e6).round() as i64)
}

/// An absolute path as a project path (`kv_render::export` resolves paths
/// starting with `/` as absolute), so sources from different directories can
/// share one project.
fn project_path(path: &Path) -> String {
    let abs = std::fs::canonicalize(path).expect("fixture exists");
    abs.to_string_lossy().replace('\\', "/")
}

/// A 60 s project over `sources` (path, duration), one media item per
/// source: segments cycle over the items, 5 s each (shorter for short
/// sources), every fourth one with a crossfade into it; the last is trimmed
/// so the edit ends at exactly 60 s. `None` when a source is too short.
fn project(sources: &[(PathBuf, Micros)], music: &Path) -> Option<Project> {
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
    let shortest = sources.iter().map(|s| s.1).min()?.as_secs_f64();
    let len = (shortest - CROSSFADE - 0.1).min(5.0);
    if len < 1.0 {
        return None;
    }
    let mut edl = Edl { segments: vec![] };
    let mut k = 0usize;
    loop {
        let (id, duration) = (k % sources.len(), sources[k % sources.len()].1);
        let fade = k % 4 == 3;
        let seg_len = if fade { len + CROSSFADE } else { len };
        // Spread the in points over the source.
        let room = duration.as_secs_f64() - seg_len;
        let src_in = (k as f64 * 0.37) % room.max(0.01);
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
        edl.repack();
        let last = edl.segments.last_mut().expect("just pushed");
        let end = last.timeline_start.0 + last.timeline_len().0;
        let over = end - us(EDIT_SECONDS).0;
        if over >= 0 {
            last.src_out = Micros(last.src_out.0 - over);
            break;
        }
        k += 1;
    }
    let sec = &mut p.sections[0];
    sec.edit = edl;
    sec.music = Some(MusicRef {
        path: project_path(music),
        duration: us(EDIT_SECONDS),
    });
    Some(p)
}

fn settings() -> ExportSettings {
    ExportSettings {
        width: 1920,
        height: 1080,
        fps: Rational::new(30, 1),
        ..ExportSettings::default()
    }
}

fn out_dir() -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("kv-render-export-bench");
    std::fs::create_dir_all(&dir).expect("create bench dir");
    dir
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1e3
}

/// Export `project` [`RUNS`] times and print its lines.
fn bench_case(backend: &FfmpegBackend, label: &str, project: &Project) {
    let out = out_dir().join(format!("{}.mp4", label.replace('/', "-")));
    let mut times = Vec::new();
    let mut last: Option<ExportReport> = None;
    for run in 0..RUNS {
        let _ = std::fs::remove_file(&out);
        let started = Instant::now();
        let report = match export(
            project,
            Project::DEFAULT_SECTION,
            Path::new("/"),
            backend,
            backend,
            &out,
            &settings(),
            &mut |_| ControlFlow::Continue(()),
        ) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("SKIPPED: export/{label}: export failed: {e}");
                return;
            }
        };
        let took = started.elapsed();
        eprintln!(
            "export/{label}: run {}: {:.2} s, {} frames, encoder {}",
            run + 1,
            took.as_secs_f64(),
            report.frames,
            report.encoder_name
        );
        times.push(took);
        last = Some(report);
        if times.iter().sum::<Duration>() >= CASE_BUDGET {
            break;
        }
    }
    let _ = std::fs::remove_file(&out);
    let report = last.expect("RUNS > 0");
    times.sort_unstable();
    let (min, median, max) = (times[0], times[times.len() / 2], times[times.len() - 1]);
    let segments = project.sections[0].edit.segments.len();
    println!(
        "export/{label} min/median/max over {} runs ({} frames, {segments} segments, encoder {})",
        times.len(),
        report.frames,
        report.encoder_name
    );
    println!(
        "                        time:   [{:.1} ms {:.1} ms {:.1} ms]  target {:.0} ms: median {}",
        ms(min),
        ms(median),
        ms(max),
        ms(TARGET),
        if median <= TARGET { "met" } else { "MISSED" }
    );
    println!(
        "TARGET export-60s-1080p-{} {:.1} {:.0}",
        label.replace('/', "-"),
        ms(median),
        ms(TARGET)
    );
}

/// Probe `path` and build the edit with `ITEMS` media items pointing at it.
fn single_source_project(
    backend: &FfmpegBackend,
    label: &str,
    path: &Path,
    music: &Path,
) -> Option<Project> {
    let info = match backend.probe(path) {
        Ok(info) => info,
        Err(e) => {
            eprintln!(
                "SKIPPED: export/{label}: cannot probe {}: {e}",
                path.display()
            );
            return None;
        }
    };
    if let Some(v) = &info.video {
        eprintln!(
            "export/{label}: source {} {}x{} {}-bit, {:.1} s",
            v.codec,
            v.width,
            v.height,
            v.bit_depth,
            info.duration.as_secs_f64()
        );
    } else {
        eprintln!("SKIPPED: export/{label}: no video stream");
        return None;
    }
    let sources = vec![(path.to_path_buf(), info.duration); ITEMS];
    let p = project(&sources, music);
    if p.is_none() {
        eprintln!("SKIPPED: export/{label}: source too short");
    }
    p
}

fn bench_corpus_file(backend: &FfmpegBackend, name: &str, music: &Path) {
    let path = kv_testkit::corpus_file_or_skip!(name);
    let label = format!("corpus/{name}");
    if let Some(p) = single_source_project(backend, &label, &path, music) {
        bench_case(backend, &label, &p);
    }
}

fn main() {
    if !std::env::args().any(|a| a == "--bench") {
        eprintln!("export bench: run with `cargo bench -p kv-render --bench export`");
        return;
    }
    let backend = FfmpegBackend::new().expect("FFmpeg initialises");
    let music = click_track(128.0, EDIT_SECONDS, 48_000);

    // (a) Tagged 1080p30 H.264 clips.
    let started = Instant::now();
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
    eprintln!(
        "export/h264-1080p30: {ITEMS} sources ready in {:.1} s",
        started.elapsed().as_secs_f64()
    );
    let p = project(&sources, &music).expect("6 s clips are long enough");
    bench_case(&backend, "h264-1080p30", &p);

    // (b) 2160p60 HEVC Main10.
    match synthetic_clip(&HEVC10_2160P60) {
        Ok(path) => {
            if let Some(p) = single_source_project(&backend, "hevc10-2160p60", &path, &music) {
                bench_case(&backend, "hevc10-2160p60", &p);
            }
        }
        Err(e) => eprintln!("SKIPPED: export/hevc10-2160p60: {e}"),
    }

    for entry in &kv_testkit::corpus_manifest().files {
        bench_corpus_file(&backend, &entry.name, &music);
    }
}
