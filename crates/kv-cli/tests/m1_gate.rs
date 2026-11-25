//! M1 gate (`docs/roadmap.md`): the `kwikvid` binary turns 10 clips and a
//! song into a beat-synced MP4, verified frame by frame.
//!
//! [`ten_clips_and_a_song_make_a_beat_synced_mp4`] runs in the merge gate on
//! Linux and macOS (`mac-smoke`, release). It renders 10 tagged clips
//! (`kv_testkit::tagged_clip_set`, every frame shows its clip and frame
//! number) cut to a 120 BPM click track, then decodes the MP4 and checks the
//! result against the `--dry-run --json` EDL and the known click times:
//! containers and codecs, duration, the source frame shown in every output
//! frame, cuts on beats, clip spread and audio sync.
//!
//! [`ten_real_gopro_clips_cut_on_beats`] does a lighter check on real footage
//! from the regression corpus (ADR 0008) and runs only in the regression
//! workflow.

use std::fs;
use std::path::Path;
use std::time::Instant;

use assert_cmd::cargo::cargo_bin_cmd;
use assert_cmd::Command;
use ffmpeg_next as ff;
use kv_media::{DecodeOptions, FfmpegBackend, MediaBackend};
use kv_testkit::{beat_times, click_track, read_frame_tag, tagged_clip_set, DEFAULT_SAMPLE_RATE};
use serde_json::Value;
use tempfile::TempDir;

const BPM: f64 = 120.0;
const SONG_SECS: f64 = 30.0;
const CLIPS: usize = 10;
const CLIP_SECS: f64 = 6.0;
/// Fixture and output frame rate.
const FPS: i64 = 30;
/// `--length 15s`, in microseconds.
const LENGTH_US: i64 = 15_000_000;
/// One beat at 120 BPM: the auto-edit's length tolerance.
const BEAT_US: i64 = 500_000;
/// One output frame, rounded up.
const FRAME_US: i64 = 1_000_000 / FPS + 1;
/// Largest distance between an audio onset and its click.
const AUDIO_SYNC_US: i64 = 20_000;
/// Whole test budget on `macos-latest` (release) and ubuntu.
const BUDGET_SECS: f64 = 60.0;

/// Arguments shared by the render and the dry run.
const EDIT_ARGS: [&str; 13] = [
    "edit", "--clips", "clips", "--music", "song.wav", "--length", "15s", "--seed", "7", "--size",
    "320x180", "--fps", "30",
];

fn kwikvid(dir: &Path, extra: &[&str]) -> Command {
    let mut cmd = cargo_bin_cmd!("kwikvid");
    cmd.current_dir(dir).args(EDIT_ARGS).args(extra);
    cmd
}

fn dry_run_json(dir: &Path) -> String {
    let out = kwikvid(dir, &["--dry-run", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    String::from_utf8(out).unwrap()
}

/// One EDL segment, with times in microseconds.
#[derive(Debug, Clone)]
struct Seg {
    /// Index of the clip in the project's media list (recording order),
    /// which is the clip index the fixture frames carry.
    clip: usize,
    src_in: i64,
    src_out: i64,
    start: i64,
    /// Percent, 100 = normal.
    speed: i64,
    crossfade: bool,
}

impl Seg {
    fn end(&self) -> i64 {
        self.start + (self.src_out - self.src_in) * 100 / self.speed.max(1)
    }

    fn covers(&self, t: i64) -> bool {
        self.start <= t && t < self.end()
    }

    /// Source time shown at timeline time `t`, clamped to the segment.
    fn src_time(&self, t: i64) -> i64 {
        (self.src_in + (t - self.start) * self.speed.max(1) / 100)
            .clamp(self.src_in, self.src_out - 1)
    }
}

/// The parsed `--dry-run --json` output.
struct Edl {
    total_length: i64,
    media_paths: Vec<String>,
    segments: Vec<Seg>,
}

impl Edl {
    fn parse(json: &str) -> Self {
        let v: Value = serde_json::from_str(json).unwrap();
        let media = v["project"]["media"].as_array().unwrap();
        let ids: Vec<u64> = media.iter().map(|m| m["id"].as_u64().unwrap()).collect();
        let segments = v["project"]["sections"][0]["edit"]["segments"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| Seg {
                clip: ids
                    .iter()
                    .position(|&id| id == s["media_id"].as_u64().unwrap())
                    .expect("segment media is in the project"),
                src_in: s["src_in"].as_i64().unwrap(),
                src_out: s["src_out"].as_i64().unwrap(),
                start: s["timeline_start"].as_i64().unwrap(),
                speed: s["speed"].as_i64().unwrap(),
                crossfade: s["transition"]["type"] == "crossfade",
            })
            .collect();
        Edl {
            total_length: v["total_length"].as_i64().unwrap(),
            media_paths: media
                .iter()
                .map(|m| m["path"].as_str().unwrap().to_owned())
                .collect(),
            segments,
        }
    }

    /// What output time `t` should show: `Some((clip, source frame))`, or
    /// `None` inside a crossfade (two segments blended).
    fn expected_at(&self, t: i64) -> Option<(usize, i64)> {
        let covering: Vec<&Seg> = self.segments.iter().filter(|s| s.covers(t)).collect();
        let seg = match covering.as_slice() {
            [] => panic!("no segment covers {t} us: the edit has a gap"),
            [.., inc] if covering.len() >= 2 && inc.crossfade => return None,
            [.., last] => *last,
        };
        Some((seg.clip, seg.src_time(t) * FPS / 1_000_000))
    }

    /// Timeline times where the picture jumps: every segment start except
    /// the first, unless it continues the previous segment seamlessly.
    fn cut_times(&self) -> Vec<i64> {
        self.segments
            .windows(2)
            .filter(|w| !(w[0].clip == w[1].clip && w[0].src_out == w[1].src_in))
            .map(|w| w[1].start)
            .collect()
    }
}

/// Presentation time of output frame `k`, as the frame plan rounds it.
fn frame_pts(k: i64) -> i64 {
    (2 * k * 1_000_000 + FPS) / (2 * FPS)
}

/// Distance from `t` (us) to the nearest of `beats` (s), in microseconds.
fn beat_distance(t: i64, beats: &[f64]) -> i64 {
    beats
        .iter()
        .map(|&b| (t - (b * 1e6).round() as i64).abs())
        .min()
        .unwrap()
}

/// Decode the first audio stream of `path` to mono `f32` samples; returns
/// the samples, the sample rate and the channel count.
fn decode_audio(path: &Path) -> (Vec<f32>, u32, u16) {
    ff::init().unwrap();
    let mut ictx = ff::format::input(path).unwrap();
    let stream = ictx.streams().best(ff::media::Type::Audio).expect("audio");
    let index = stream.index();
    let mut decoder = ff::codec::context::Context::from_parameters(stream.parameters())
        .and_then(|c| c.decoder().audio())
        .unwrap();
    let mut mono = Vec::new();
    let mut rate = 0;
    let mut channels = 0;
    let mut frame = ff::frame::Audio::empty();
    let mut receive = |decoder: &mut ff::decoder::Audio, mono: &mut Vec<f32>| {
        while decoder.receive_frame(&mut frame).is_ok() {
            rate = frame.rate();
            channels = frame.channels();
            let (n, ch) = (frame.samples(), usize::from(frame.channels()));
            let f32s = |bytes: &[u8], len: usize| -> Vec<f32> {
                bytes[..len * 4]
                    .chunks_exact(4)
                    .map(|b| f32::from_ne_bytes(b.try_into().unwrap()))
                    .collect()
            };
            match frame.format() {
                ff::format::Sample::F32(ff::format::sample::Type::Planar) => {
                    let planes: Vec<Vec<f32>> = (0..ch).map(|c| f32s(frame.data(c), n)).collect();
                    mono.extend(
                        (0..n).map(|i| planes.iter().map(|p| p[i]).sum::<f32>() / ch as f32),
                    );
                }
                ff::format::Sample::F32(ff::format::sample::Type::Packed) => {
                    let all = f32s(frame.data(0), n * ch);
                    mono.extend(
                        all.chunks_exact(ch)
                            .map(|f| f.iter().sum::<f32>() / ch as f32),
                    );
                }
                other => panic!("unexpected decoded sample format {other:?}"),
            }
        }
    };
    for (s, packet) in ictx.packets() {
        if s.index() == index {
            decoder.send_packet(&packet).unwrap();
            receive(&mut decoder, &mut mono);
        }
    }
    decoder.send_eof().unwrap();
    receive(&mut decoder, &mut mono);
    (mono, rate, channels)
}

/// Click onsets in `samples`, in microseconds: an RMS envelope (2 ms
/// windows, 0.5 ms hop) is split into bursts above a small floor; each
/// burst's onset is where it first reaches half its peak.
fn onsets(samples: &[f32], rate: u32) -> Vec<i64> {
    let win = (rate / 500) as usize;
    let hop = (rate / 2000) as usize;
    let env: Vec<f32> = (0..samples.len().saturating_sub(win) / hop)
        .map(|i| {
            let w = &samples[i * hop..i * hop + win];
            (w.iter().map(|x| x * x).sum::<f32>() / win as f32).sqrt()
        })
        .collect();
    let floor = 0.01;
    // Bursts closer than this belong to the same click.
    let min_gap = (0.1 * f64::from(rate)) as usize / hop;
    let mut out = Vec::new();
    let mut i = 0;
    while i < env.len() {
        if env[i] < floor {
            i += 1;
            continue;
        }
        let start = i;
        let mut last_loud = i;
        while i < env.len() && i - last_loud < min_gap {
            if env[i] >= floor {
                last_loud = i;
            }
            i += 1;
        }
        let burst = &env[start..=last_loud];
        let peak = burst.iter().copied().fold(0.0, f32::max);
        let at = start + burst.iter().position(|&e| e >= peak / 2.0).unwrap();
        // The window centre is the sample time of an envelope value.
        let sample = at * hop + win / 2;
        out.push((sample as f64 * 1e6 / f64::from(rate)).round() as i64);
    }
    out
}

#[test]
fn ten_clips_and_a_song_make_a_beat_synced_mp4() {
    let started = Instant::now();
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    fs::create_dir(d.join("clips")).unwrap();
    for src in tagged_clip_set(CLIPS, CLIP_SECS) {
        fs::copy(&src, d.join("clips").join(src.file_name().unwrap())).unwrap();
    }
    fs::copy(
        click_track(BPM, SONG_SECS, DEFAULT_SAMPLE_RATE),
        d.join("song.wav"),
    )
    .unwrap();
    let beats = beat_times(BPM, SONG_SECS);

    // Determinism: the same seed gives the same EDL.
    let json = dry_run_json(d);
    assert_eq!(json, dry_run_json(d), "dry run is deterministic per seed");
    let edl = Edl::parse(&json);
    let expected_names: Vec<String> = (1..=CLIPS)
        .map(|i| format!("clips/GX01{i:04}.MP4"))
        .collect();
    assert_eq!(edl.media_paths, expected_names, "clips in recording order");

    let render_started = Instant::now();
    let out = kwikvid(d, &["--out", "out.mp4"])
        .assert()
        .success()
        .get_output()
        .clone();
    let render_secs = render_started.elapsed().as_secs_f64();
    let stderr = String::from_utf8(out.stderr).unwrap();
    let summary = stderr.lines().last().unwrap_or_default();
    let encoder = summary
        .split_once(", encoder ")
        .and_then(|(_, r)| r.split(',').next())
        .unwrap_or_else(|| panic!("no encoder in summary {summary:?}"));
    println!("m1 gate: encoder {encoder}, render {render_secs:.1} s");

    // Container, codecs, duration.
    let path = d.join("out.mp4");
    let backend = FfmpegBackend::new().unwrap();
    let info = backend.probe(&path).unwrap();
    assert!(info.container.contains("mp4"), "{}", info.container);
    let video = info.video.as_ref().expect("video stream");
    let audio = info.audio.as_ref().expect("audio stream");
    assert_eq!(video.codec, "h264");
    assert_eq!((video.width, video.height), (320, 180));
    assert_eq!(audio.codec, "aac");
    assert_eq!(audio.channels, 2, "stereo");
    let duration = info.duration.0;
    assert!(
        (duration - LENGTH_US).abs() <= BEAT_US,
        "duration {duration} us is not 15 s +- one beat"
    );
    assert!(
        (duration - edl.total_length).abs() <= FRAME_US,
        "duration {duration} us differs from the EDL's {} us by more than a frame",
        edl.total_length
    );

    // Every frame shows what the EDL says, and cuts land on beats.
    let mut reader = backend
        .open_video(&path, &DecodeOptions::default())
        .unwrap();
    let mut k = 0i64;
    let mut prev: Option<(usize, i64)> = None;
    let mut detected_cuts = Vec::new();
    let mut seen_clips = std::collections::BTreeSet::new();
    while let Some(frame) = reader.next_frame().unwrap() {
        let t = frame_pts(k);
        assert!(
            (frame.pts.0 - t).abs() <= 1_000,
            "frame {k} at {} us, expected {t} us",
            frame.pts.0
        );
        let tag = read_frame_tag(&frame.data, frame.width, frame.height)
            .map(|tag| (usize::from(tag.clip), i64::from(tag.frame)));
        if let Some((clip, src)) = edl.expected_at(t) {
            let (got_clip, got_src) =
                tag.unwrap_or_else(|| panic!("output frame {k} ({t} us) has no tag"));
            assert_eq!(got_clip, clip, "clip of output frame {k} ({t} us)");
            assert!(
                (got_src - src).abs() <= 1,
                "output frame {k} ({t} us) shows source frame {got_src}, EDL says {src}"
            );
        }
        if let Some((clip, src)) = tag {
            seen_clips.insert(clip);
            if let Some((pclip, psrc)) = prev {
                if clip != pclip || !(0..=2).contains(&(src - psrc)) {
                    detected_cuts.push(t);
                }
            }
            prev = tag;
        }
        k += 1;
    }
    let edl_frames = (edl.total_length * FPS + 999_999) / 1_000_000;
    assert!(
        (k - edl_frames).abs() <= 1,
        "{k} frames decoded, the EDL has {edl_frames}"
    );
    println!("m1 gate: {k} frames, cuts at {detected_cuts:?} us");
    assert_eq!(
        detected_cuts.len(),
        edl.cut_times().len(),
        "cuts seen in the video vs EDL cuts {:?}",
        edl.cut_times()
    );
    for &t in &detected_cuts {
        let off = beat_distance(t, &beats);
        assert!(
            off <= FRAME_US,
            "cut at {t} us is {off} us from the nearest beat"
        );
    }
    assert!(
        seen_clips.len() >= 5,
        "only clips {seen_clips:?} appear in the edit"
    );

    // The music is in sync: every click in the output is where the song has
    // it, and every onset is a click.
    let (samples, rate, channels) = decode_audio(&path);
    assert_eq!(channels, 2);
    let found = onsets(&samples, rate);
    for &o in &found {
        let off = beat_distance(o, &beats);
        assert!(
            off <= AUDIO_SYNC_US,
            "audio onset at {o} us is {off} us from the nearest click"
        );
    }
    // The last second fades out; clicks before it must all be heard.
    let audible: Vec<f64> = beats
        .iter()
        .copied()
        .filter(|&b| b * 1e6 < (edl.total_length - 1_000_000) as f64)
        .collect();
    for &b in &audible {
        let t = (b * 1e6).round() as i64;
        assert!(
            found.iter().any(|&o| (o - t).abs() <= AUDIO_SYNC_US),
            "no audio onset near the click at {t} us (onsets {found:?})"
        );
    }

    let total = started.elapsed().as_secs_f64();
    println!("m1 gate: total wall time {total:.1} s");
    assert!(
        total < BUDGET_SECS,
        "the gate test took {total:.1} s, budget {BUDGET_SECS} s"
    );
}

/// Corpus files used by [`ten_real_gopro_clips_cut_on_beats`]: manifest
/// entries named `m1-gate-*`, in name order.
const CORPUS_PREFIX: &str = "m1-gate-";

#[test]
#[ignore = "regression"]
fn ten_real_gopro_clips_cut_on_beats() {
    let mut names: Vec<&str> = kv_testkit::corpus_manifest()
        .files
        .iter()
        .map(|e| e.name.as_str())
        .filter(|n| n.starts_with(CORPUS_PREFIX))
        .collect();
    names.sort_unstable();
    if names.len() < CLIPS {
        eprintln!(
            "SKIPPED: fixtures/corpus.toml lists {} of the {CLIPS} `{CORPUS_PREFIX}*` clips",
            names.len()
        );
        return;
    }
    let dir: TempDir = tempfile::tempdir().unwrap();
    let d = dir.path();
    fs::create_dir(d.join("clips")).unwrap();
    for name in &names[..CLIPS] {
        let src = kv_testkit::corpus_file_or_skip!(name);
        // Hard links keep large footage off the temp disk where possible.
        let dst = d.join("clips").join(name);
        if fs::hard_link(&src, &dst).is_err() {
            fs::copy(&src, &dst).unwrap();
        }
    }
    let song = click_track(BPM, SONG_SECS, DEFAULT_SAMPLE_RATE);
    fs::copy(&song, d.join("song.wav")).unwrap();

    let edl = Edl::parse(&dry_run_json(d));
    let started = Instant::now();
    kwikvid(d, &["--out", "out.mp4"]).assert().success();
    println!(
        "m1 gate (corpus): render {:.1} s",
        started.elapsed().as_secs_f64()
    );

    let info = FfmpegBackend::new()
        .unwrap()
        .probe(&d.join("out.mp4"))
        .unwrap();
    assert!(
        (info.duration.0 - LENGTH_US).abs() <= BEAT_US,
        "duration {:?}",
        info.duration
    );
    assert!((info.duration.0 - edl.total_length).abs() <= FRAME_US);

    // Every segment boundary is on a beat the analysis found.
    let analysis = kv_audio::analyze(&song).unwrap();
    let beats: Vec<f64> = analysis.beats.iter().map(|b| b.0 as f64 / 1e6).collect();
    for seg in edl.segments.iter().skip(1) {
        let off = beat_distance(seg.start, &beats);
        assert!(
            off <= FRAME_US,
            "segment at {} us is {off} us off the beat",
            seg.start
        );
    }
}
