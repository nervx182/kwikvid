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
//! - `hevc10-2160p60`: one 6 s 2160p60 HEVC Main10 clip, generated like the
//!   HEVC case of `crates/kv-media/benches/seek.rs` (`libx265` or
//!   `hevc_videotoolbox`; skipped with a `SKIPPED:` line when the FFmpeg
//!   build has neither) into `CARGO_TARGET_TMPDIR`. The 12 media items all
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
//! case runs [`RUNS`] exports and reports the minimum and the median, which
//! keeps the whole bench well under 10 minutes on the Mac runner. The first
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

use ffmpeg_next as ff;
use kv_core::{
    Edl, MediaId, MediaItem, MediaKind, Micros, MusicRef, Project, ProjectId, Segment, Speed,
    Transition,
};
use kv_media::{ExportSettings, FfmpegBackend, MediaBackend, Rational};
use kv_render::{export, ExportReport};
use kv_testkit::{click_track, tagged_clip, ClipSpec};

/// The Roadmap I export budget.
const TARGET: Duration = Duration::from_secs(20);
/// Exports per case.
const RUNS: usize = 3;
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
    }
    let _ = std::fs::remove_file(&out);
    let report = last.expect("RUNS > 0");
    times.sort_unstable();
    let (min, median, max) = (times[0], times[times.len() / 2], times[times.len() - 1]);
    let segments = project.sections[0].edit.segments.len();
    println!(
        "export/{label} min/median/max over {RUNS} runs ({} frames, {segments} segments, encoder {})",
        report.frames, report.encoder_name
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
    match hevc10_clip() {
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

// --- 2160p60 HEVC Main10 fixture, as in crates/kv-media/benches/seek.rs ---

const HEVC_WIDTH: u32 = 3840;
const HEVC_HEIGHT: u32 = 2160;
const HEVC_FPS: i32 = 60;
const HEVC_GOP: u32 = 60;
/// Bump when the generated content or encoder settings change.
const HEVC_FIXTURE_VERSION: u32 = 1;

/// Path of the HEVC Main10 clip, encoding it on first use. `Err` explains
/// why no encoder could produce it.
fn hevc10_clip() -> Result<PathBuf, String> {
    let path = out_dir().join(format!("hevc10-2160p60-gop60-v{HEVC_FIXTURE_VERSION}.mp4"));
    if path.exists() {
        return Ok(path);
    }
    let tmp = path.with_extension("mp4.tmp");
    let started = Instant::now();
    let encoder = encode_hevc10(&tmp)?;
    std::fs::rename(&tmp, &path).expect("move fixture into place");
    eprintln!(
        "generated {} with {encoder} in {:.1} s",
        path.display(),
        started.elapsed().as_secs_f64()
    );
    Ok(path)
}

/// Open the first usable 10-bit HEVC encoder.
fn open_hevc_encoder(global_header: bool) -> Result<(ff::Codec, ff::encoder::Video), String> {
    use ff::format::Pixel;
    let keyint = HEVC_GOP.to_string();
    let candidates: [(&str, Pixel, Vec<(&str, String)>); 2] = [
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
    ];
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
        enc.set_width(HEVC_WIDTH);
        enc.set_height(HEVC_HEIGHT);
        enc.set_format(pixel);
        enc.set_time_base(ff::Rational::new(1, HEVC_FPS));
        enc.set_frame_rate(Some(ff::Rational::new(HEVC_FPS, 1)));
        enc.set_gop(HEVC_GOP);
        enc.set_colorspace(ff::color::Space::BT709);
        enc.set_color_range(ff::color::Range::MPEG);
        // 0.1 bit per pixel, about 50 Mb/s: roughly camera-like.
        let pixels_per_sec = f64::from(HEVC_WIDTH) * f64::from(HEVC_HEIGHT) * f64::from(HEVC_FPS);
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

/// Encode the HEVC clip at `path`; returns the encoder's name.
fn encode_hevc10(path: &Path) -> Result<String, String> {
    ff::init().map_err(|e| e.to_string())?;
    let mut octx = ff::format::output_as(path, "mp4").map_err(|e| e.to_string())?;
    let global_header = octx
        .format()
        .flags()
        .contains(ff::format::Flags::GLOBAL_HEADER);
    let (codec, mut enc) = open_hevc_encoder(global_header)?;
    let enc_tb = ff::Rational::new(1, HEVC_FPS);
    {
        let mut ost = octx.add_stream(codec).map_err(|e| e.to_string())?;
        ost.set_parameters(&enc);
        ost.set_time_base(enc_tb);
        ost.set_avg_frame_rate(ff::Rational::new(HEVC_FPS, 1));
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
    let mut frame = ff::frame::Video::new(enc.format(), HEVC_WIDTH, HEVC_HEIGHT);
    let frames = (f64::from(HEVC_FPS) * CLIP_SECONDS) as i32;
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

/// Draw frame `i` of a 10-bit picture: scrolling diagonal gradients, a moving
/// block and light noise, so P frames carry real residuals.
fn fill_frame(frame: &mut ff::frame::Video, i: u32) {
    use ff::format::Pixel;
    let (w, h) = (frame.width() as usize, frame.height() as usize);
    let p010 = frame.format() == Pixel::P010LE;
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
        // 8-bit value scaled to 10 bits; P010 is MSB-aligned.
        let v = if p010 {
            (v as u16) << 8
        } else {
            (v as u16) << 2
        };
        data[o..o + 2].copy_from_slice(&v.to_le_bytes());
    };

    let stride = frame.stride(0);
    let y_plane = frame.data_mut(0);
    for y in 0..h {
        for x in 0..w {
            put(y_plane, y * stride + x * 2, luma(x, y, noise()));
        }
    }
    let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
    if p010 {
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
                    put(data, y * stride + x * 2, chroma(x, y, shift));
                }
            }
        }
    }
}
