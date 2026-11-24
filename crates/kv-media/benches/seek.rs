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
//! so the regression job summary picks it up.
//!
//! Fixtures are encoded with FFmpeg on first run into `CARGO_TARGET_TMPDIR`
//! and reused by later runs. Runs only in the regression `benchmarks` job:
//! `cargo bench -p kv-media --bench seek`.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use criterion::{criterion_group, criterion_main, Criterion, SamplingMode};
use ffmpeg_next as ff;
use kv_core::Micros;
use kv_media::{DecodeOptions, FfmpegBackend, MediaBackend};

/// The M2 seek target this benchmark checks.
const TARGET: Duration = Duration::from_millis(150);
/// The reduced preview size measured next to full size.
const PREVIEW_SIZE: (u32, u32) = (960, 540);

/// A synthetic clip to generate.
struct Synth {
    name: &'static str,
    width: u32,
    height: u32,
    fps: i32,
    seconds: i32,
    gop: u32,
    ten_bit: bool,
}

const SYNTH: [Synth; 3] = [
    Synth {
        name: "h264-1080p30-gop30",
        width: 1920,
        height: 1080,
        fps: 30,
        seconds: 20,
        gop: 30,
        ten_bit: false,
    },
    Synth {
        name: "h264-1080p30-gop300",
        width: 1920,
        height: 1080,
        fps: 30,
        seconds: 20,
        gop: 300,
        ten_bit: false,
    },
    Synth {
        name: "hevc10-2160p60-gop60",
        width: 3840,
        height: 2160,
        fps: 60,
        seconds: 6,
        gop: 60,
        ten_bit: true,
    },
];

/// Bump when the generated content or encoder settings change, so cached
/// fixtures from an older run are not reused.
const FIXTURE_VERSION: u32 = 1;

fn fixture_dir() -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("kv-media-seek");
    std::fs::create_dir_all(&dir).expect("create fixture dir");
    dir
}

/// Path of `s`, encoding it on first use. `Err` explains why no encoder could
/// produce it.
fn synth_clip(s: &Synth) -> Result<PathBuf, String> {
    let path = fixture_dir().join(format!("{}-v{FIXTURE_VERSION}.mp4", s.name));
    if path.exists() {
        return Ok(path);
    }
    let tmp = path.with_extension("mp4.tmp");
    let started = Instant::now();
    let encoder = encode_synth(&tmp, s)?;
    std::fs::rename(&tmp, &path).expect("move fixture into place");
    eprintln!(
        "generated {} with {encoder} in {:.1} s",
        path.display(),
        started.elapsed().as_secs_f64()
    );
    Ok(path)
}

/// An encoder to try: name, input pixel format, private options.
type Candidate = (&'static str, ff::format::Pixel, Vec<(&'static str, String)>);

/// Open the first usable encoder for `s`.
fn open_encoder(s: &Synth, global_header: bool) -> Result<(ff::Codec, ff::encoder::Video), String> {
    use ff::format::Pixel;
    let keyint = s.gop.to_string();
    let candidates: Vec<Candidate> = if s.ten_bit {
        vec![
            (
                "libx265",
                Pixel::YUV420P10LE,
                vec![
                    ("preset", "ultrafast".into()),
                    (
                        "x265-params",
                        format!("log-level=error:keyint={keyint}:min-keyint={keyint}:scenecut=0"),
                    ),
                ],
            ),
            (
                "hevc_videotoolbox",
                Pixel::P010LE,
                vec![("profile", "main10".into())],
            ),
        ]
    } else {
        vec![
            (
                "libx264",
                Pixel::YUV420P,
                vec![
                    ("preset", "ultrafast".into()),
                    (
                        "x264-params",
                        format!("keyint={keyint}:min-keyint={keyint}:scenecut=0"),
                    ),
                ],
            ),
            ("h264_videotoolbox", Pixel::YUV420P, vec![]),
            ("libopenh264", Pixel::YUV420P, vec![]),
        ]
    };
    let mut tried = Vec::new();
    for (name, pixel, private) in candidates {
        let Some(codec) = ff::encoder::find_by_name(name) else {
            tried.push(format!("{name}: not built in"));
            continue;
        };
        let mut enc = ff::codec::Context::new_with_codec(codec)
            .encoder()
            .video()
            .map_err(|e| format!("{name}: {e}"))?;
        enc.set_width(s.width);
        enc.set_height(s.height);
        enc.set_format(pixel);
        enc.set_time_base(ff::Rational::new(1, s.fps));
        enc.set_frame_rate(Some(ff::Rational::new(s.fps, 1)));
        enc.set_gop(s.gop);
        enc.set_colorspace(ff::color::Space::BT709);
        enc.set_color_range(ff::color::Range::MPEG);
        // Roughly camera-like: 0.1 bit per pixel (about 6 Mb/s at 1080p30,
        // 50 Mb/s at 2160p60). Only rate-controlled encoders use it.
        let pixels_per_sec = f64::from(s.width) * f64::from(s.height) * f64::from(s.fps);
        enc.set_bit_rate((pixels_per_sec * 0.1) as usize);
        if global_header {
            enc.set_flags(ff::codec::Flags::GLOBAL_HEADER);
        }
        let mut opts = ff::Dictionary::new();
        for (k, v) in &private {
            opts.set(k, v);
        }
        match enc.open_with(opts) {
            Ok(opened) => return Ok((codec, opened)),
            Err(e) => tried.push(format!("{name}: {e}")),
        }
    }
    Err(format!("no usable encoder ({})", tried.join("; ")))
}

/// Encode `s` as an MP4 at `path`; returns the encoder's name.
fn encode_synth(path: &Path, s: &Synth) -> Result<String, String> {
    ff::init().map_err(|e| e.to_string())?;
    let mut octx = ff::format::output_as(path, "mp4").map_err(|e| e.to_string())?;
    let global_header = octx
        .format()
        .flags()
        .contains(ff::format::Flags::GLOBAL_HEADER);
    let (codec, mut enc) = open_encoder(s, global_header)?;
    let enc_tb = ff::Rational::new(1, s.fps);
    {
        let mut ost = octx.add_stream(codec).map_err(|e| e.to_string())?;
        ost.set_parameters(&enc);
        ost.set_time_base(enc_tb);
        ost.set_avg_frame_rate(ff::Rational::new(s.fps, 1));
    }
    octx.write_header().map_err(|e| e.to_string())?;

    let drain = |enc: &mut ff::encoder::Video, octx: &mut ff::format::context::Output| {
        let mut pkt = ff::Packet::empty();
        while enc.receive_packet(&mut pkt).is_ok() {
            pkt.set_stream(0);
            pkt.rescale_ts(enc_tb, octx.stream(0).expect("stream").time_base());
            pkt.write_interleaved(octx).expect("write packet");
        }
    };
    let mut frame = ff::frame::Video::new(enc.format(), s.width, s.height);
    let frames = s.fps * s.seconds;
    for i in 0..frames {
        fill_frame(&mut frame, i as u32);
        frame.set_pts(Some(i64::from(i)));
        enc.send_frame(&frame).map_err(|e| e.to_string())?;
        drain(&mut enc, &mut octx);
    }
    enc.send_eof().map_err(|e| e.to_string())?;
    drain(&mut enc, &mut octx);
    octx.write_trailer().map_err(|e| e.to_string())?;
    Ok(codec.name().to_string())
}

/// Draw frame `i`: diagonal gradients that scroll at different speeds, a
/// moving block and light per-pixel noise, so every frame differs and P
/// frames carry real residuals (a flat or static picture would make
/// decoding unrealistically cheap).
fn fill_frame(frame: &mut ff::frame::Video, i: u32) {
    use ff::format::Pixel;
    let (w, h) = (frame.width() as usize, frame.height() as usize);
    let format = frame.format();
    let ten_bit = matches!(format, Pixel::YUV420P10LE | Pixel::P010LE);
    let mut seed = 0x9E37_79B9_u32 ^ i.wrapping_mul(0x85EB_CA6B);
    let mut noise = move || {
        seed ^= seed << 13;
        seed ^= seed >> 17;
        seed ^= seed << 5;
        (seed & 15) as i32 - 8
    };
    let (bx, by) = ((i as usize * 13) % w, (i as usize * 7) % h);
    let luma = |x: usize, y: usize, n: i32| -> i32 {
        let mut v = ((x + y + i as usize * 4) % 512) as i32 / 2 + n;
        if x.abs_diff(bx) < w / 8 && y.abs_diff(by) < h / 8 {
            v = 235 - v / 4;
        }
        v.clamp(16, 235)
    };
    let chroma = |x: usize, y: usize, shift: usize| -> i32 {
        (64 + ((x * 2 + shift + i as usize * 3) % 256) as i32 / 2 + (y % 64) as i32).clamp(16, 240)
    };

    let put = |data: &mut [u8], o: usize, v: i32| {
        if ten_bit {
            let v = if format == Pixel::P010LE {
                (v as u16) << 8 // 10 significant bits, MSB-aligned
            } else {
                (v as u16) << 2
            };
            data[o..o + 2].copy_from_slice(&v.to_le_bytes());
        } else {
            data[o] = v as u8;
        }
    };
    let bytes = if ten_bit { 2 } else { 1 };

    let stride = frame.stride(0);
    let y_plane = frame.data_mut(0);
    for y in 0..h {
        for x in 0..w {
            put(y_plane, y * stride + x * bytes, luma(x, y, noise()));
        }
    }
    let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
    if format == Pixel::P010LE {
        let stride = frame.stride(1);
        let uv = frame.data_mut(1);
        for y in 0..ch {
            for x in 0..cw {
                put(uv, y * stride + x * 4, chroma(x, y, 0));
                put(uv, y * stride + x * 4 + 2, chroma(x, y, 97));
            }
        }
    } else {
        for (plane, shift) in [(1, 0), (2, 97)] {
            let stride = frame.stride(plane);
            let data = frame.data_mut(plane);
            for y in 0..ch {
                for x in 0..cw {
                    put(data, y * stride + x * bytes, chroma(x, y, shift));
                }
            }
        }
    }
}

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
        let mut reader = match backend.open_video(path, &opts) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("SKIPPED: seek/{label}/{size_label}: {e}");
                continue;
            }
        };
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
        println!("seek/{id} p50/p95/max over {} calls", samples.len());
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
        match synth_clip(s) {
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
