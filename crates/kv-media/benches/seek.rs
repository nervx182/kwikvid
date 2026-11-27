//! Random-seek latency of `VideoReader::frame_at` (issue #53), measured
//! against the M2 target "seek under 150 ms" (`docs/roadmap.md`, MVP item 10)
//! to decide whether preview proxies are needed.
//!
//! Cases, each decoded at full size and at 960x540 (`DecodeOptions.size`):
//! - generated 1080p30 H.264, GOP 30 and GOP 300;
//! - generated 2160p60 HEVC Main10, GOP 60 (skipped with a message when the
//!   FFmpeg build has no 10-bit HEVC encoder);
//! - every regression-corpus clip (`fixtures/corpus.toml`, ADR 0008) that is
//!   checked out; missing ones are skipped by `corpus_file_or_skip!`.
//!
//! Every measured call asks for a pseudo-random time anywhere in the clip on
//! one open reader, like a user clicking around the timeline. Besides
//! criterion's mean, each case prints one line with the p50 / p95 / max of
//! all its individual calls (warm-up included), in criterion's `time:` style
//! so the regression job summary picks it up, and which decode path the
//! reader uses (`hardware` = VideoToolbox, `cpu`; `KV_HWDECODE=0` forces the
//! CPU for before/after comparisons).
//!
//! Fixtures are encoded with FFmpeg on first run by
//! [`kv_testkit::synthetic_clip`] into `kv_testkit::fixtures_dir()` and
//! reused by later runs (the HEVC clip is shared with the export bench).
//! Runs only in the regression `benchmarks` job:
//! `cargo bench -p kv-media --bench seek`.

use std::path::Path;
use std::time::{Duration, Instant};

use criterion::{criterion_group, criterion_main, Criterion, SamplingMode};
use kv_core::Micros;
use kv_media::{DecodeOptions, FfmpegBackend, MediaBackend, VideoReader};
use kv_testkit::{synthetic_clip, SynthSpec};

/// The M2 seek target this benchmark checks.
const TARGET: Duration = Duration::from_millis(150);
/// The reduced preview size measured next to full size.
const PREVIEW_SIZE: (u32, u32) = (960, 540);

/// A synthetic clip to generate, with its bench label.
struct Synth {
    name: &'static str,
    spec: SynthSpec,
}

const SYNTH: [Synth; 3] = [
    Synth {
        name: "h264-1080p30-gop30",
        spec: SynthSpec {
            width: 1920,
            height: 1080,
            fps: 30,
            seconds: 20,
            gop: 30,
            ten_bit: false,
        },
    },
    Synth {
        name: "h264-1080p30-gop300",
        spec: SynthSpec {
            width: 1920,
            height: 1080,
            fps: 30,
            seconds: 20,
            gop: 300,
            ten_bit: false,
        },
    },
    Synth {
        name: "hevc10-2160p60-gop60",
        spec: kv_testkit::HEVC10_2160P60,
    },
];

/// Deterministic pseudo-random times in `[0, duration)`.
struct SeekTimes(u64);

impl SeekTimes {
    fn next(&mut self, duration: Micros) -> Micros {
        // SplitMix64.
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^= z >> 31;
        Micros((z % duration.0.max(1) as u64) as i64)
    }
}

fn percentile(sorted: &[Duration], p: f64) -> Duration {
    let rank = ((p / 100.0) * sorted.len() as f64).ceil() as usize;
    sorted[rank.clamp(1, sorted.len()) - 1]
}

fn ms(d: Duration) -> String {
    format!("{:.2} ms", d.as_secs_f64() * 1e3)
}

/// Benchmark random `frame_at` on `path` at full size and at 960x540.
fn bench_file(c: &mut Criterion, backend: &FfmpegBackend, label: &str, path: &Path) {
    let info = match backend.probe(path) {
        Ok(info) => info,
        Err(e) => {
            eprintln!(
                "SKIPPED: seek/{label}: cannot probe {}: {e}",
                path.display()
            );
            return;
        }
    };
    let Some(video) = info.video else {
        eprintln!("SKIPPED: seek/{label}: no video stream");
        return;
    };
    eprintln!(
        "seek/{label}: {} {}x{} {}-bit, {:.1} s",
        video.codec,
        video.width,
        video.height,
        video.bit_depth,
        info.duration.0 as f64 / 1e6
    );

    let mut group = c.benchmark_group("seek");
    // Seeks on 4K HEVC take a good fraction of a second: flat sampling keeps
    // the iteration count (and the run time) bounded.
    group.sampling_mode(SamplingMode::Flat);
    group.sample_size(10);
    group.warm_up_time(Duration::from_secs(1));
    group.measurement_time(Duration::from_secs(10));

    for (size_label, size) in [("full", None), ("960x540", Some(PREVIEW_SIZE))] {
        let opts = DecodeOptions {
            size,
            ..DecodeOptions::default()
        };
        let mut reader = match backend.open_video_reader(path, &opts) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("SKIPPED: seek/{label}/{size_label}: {e}");
                continue;
            }
        };
        let decode = if reader.hardware() { "hardware" } else { "cpu" };
        eprintln!("seek/{label}/{size_label}: {decode} decode");
        let mut times = SeekTimes(0x5EEC_0000 ^ label.len() as u64);
        let mut samples: Vec<Duration> = Vec::new();
        let id = format!("{label}/{size_label}");
        group.bench_function(&id, |b| {
            b.iter_custom(|iters| {
                let mut total = Duration::ZERO;
                for _ in 0..iters {
                    let t = times.next(info.duration);
                    let started = Instant::now();
                    let frame = reader.frame_at(t).expect("frame_at");
                    let took = started.elapsed();
                    std::hint::black_box(frame);
                    samples.push(took);
                    total += took;
                }
                total
            })
        });
        if samples.is_empty() {
            continue;
        }
        samples.sort_unstable();
        let (p50, p95, max) = (
            percentile(&samples, 50.0),
            percentile(&samples, 95.0),
            *samples.last().expect("non-empty"),
        );
        // Same shape as criterion's own lines so the job summary's
        // `grep -B1 'time:'` shows it.
        println!(
            "seek/{id} p50/p95/max over {} calls, {decode} decode",
            samples.len()
        );
        println!(
            "                        time:   [{} {} {}]  target {}: p95 {}",
            ms(p50),
            ms(p95),
            ms(max),
            ms(TARGET),
            if p95 <= TARGET { "met" } else { "MISSED" }
        );
    }
    group.finish();
}

fn seek(c: &mut Criterion) {
    let backend = FfmpegBackend::new().expect("FFmpeg initialises");
    for s in &SYNTH {
        match synthetic_clip(&s.spec) {
            Ok(path) => bench_file(c, &backend, s.name, &path),
            Err(e) => eprintln!("SKIPPED: seek/{}: {e}", s.name),
        }
    }
    for entry in &kv_testkit::corpus_manifest().files {
        bench_corpus_file(c, &backend, &entry.name);
    }
}

/// One regression-corpus clip; skipped (with a `SKIPPED:` line) when it is
/// not checked out.
fn bench_corpus_file(c: &mut Criterion, backend: &FfmpegBackend, name: &str) {
    let path = kv_testkit::corpus_file_or_skip!(name);
    bench_file(c, backend, &format!("corpus/{name}"), &path);
}

criterion_group!(benches, seek);
criterion_main!(benches);
