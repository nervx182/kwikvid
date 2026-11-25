//! The M1 milestone gate (`docs/roadmap.md`): the `kwikvid` CLI turns 10 clips
//! and a song into a beat-synced MP4.
//!
//! [`ten_clips_and_a_song_make_a_beat_synced_mp4`] runs the real binary on 10
//! generated tagged clips (`kv_testkit::tagged_clip_set`, every frame carries
//! its clip and frame number) and a 120 BPM click track, decodes the MP4 and
//! proves frame by frame that it is the edit the dry run promised, cut on the
//! beat, with the music in sync. It runs in the merge gate on Linux and on
//! macOS (`mac-smoke`, release build).
//!
//! [`ten_real_gopro_clips_cut_on_beats`] is the same command on real footage
//! from the regression corpus; it skips until the owner adds the clips (#106).

use std::fs;
use std::path::Path;
use std::time::Instant;

use assert_cmd::cargo::cargo_bin_cmd;
use assert_cmd::Command;
use ffmpeg_next as ff;
use kv_media::{DecodeOptions, FfmpegBackend, MediaBackend, PixelFormat};
use kv_testkit::{beat_times, click_track, read_frame_tag, tagged_clip_set, DEFAULT_SAMPLE_RATE};
use serde_json::Value;

const CLIPS: usize = 10;
const CLIP_SECS: f64 = 6.0;
const BPM: f64 = 120.0;
const SONG_SECS: f64 = 30.0;
const LENGTH: &str = "15s";
const LENGTH_US: i64 = 15_000_000;
const SEED: &str = "7";
const FPS: i64 = 30;
/// One output frame at 30 fps, in microseconds.
const FRAME_US: i64 = 1_000_000 / FPS;
/// One beat at 120 BPM: the auto-edit's length tolerance (#8).
const BEAT_US: i64 = 500_000;
/// How far an audible click may be from its true time in the output.
const AUDIO_SYNC_US: i64 = 20_000;

/// One segment of the dry-run EDL, all times in microseconds.
#[derive(Debug, Clone, Copy)]
struct Seg {
    /// Index of the media item in the project (= tagged clip index).
    clip: usize,
    src_in: i64,
    src_out: i64,
    timeline_start: i64,
    /// Crossfade overlap with the previous segment (0 for a cut).
    fade: i64,
    /// Speed in percent (100 = normal).
    speed: i64,
}

impl Seg {
    fn timeline_end(&self) -> i64 {
        self.timeline_start + (self.src_out - self.src_in) * 100 / self.speed
    }
}

fn kwikvid(dir: &Path) -> Command {
    let mut cmd = cargo_bin_cmd!("kwikvid");
    cmd.current_dir(dir);
    cmd
}

/// `kwikvid edit --clips clips --music song.wav --length 15s --seed 7 ...`.
fn edit(dir: &Path, extra: &[&str]) -> Command {
    let mut cmd = kwikvid(dir);
    cmd.args([
        "edit", "--clips", "clips", "--music", "song.wav", "--length", LENGTH, "--seed", SEED,
    ])
    .args(extra);
    cmd
}

fn dry_run_json(dir: &Path) -> String {
    let out = edit(dir, &["--dry-run", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    String::from_utf8(out).unwrap()
}

/// The segments of the dry-run JSON, with media ids mapped to media order.
fn segments(edl: &Value) -> Vec<Seg> {
    let media: Vec<u64> = edl["project"]["media"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["id"].as_u64().unwrap())
        .collect();
    edl["project"]["sections"][0]["edit"]["segments"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| {
            let id = s["media_id"].as_u64().unwrap();
            let t = &s["transition"];
            Seg {
                clip: media.iter().position(|&m| m == id).unwrap(),
                src_in: s["src_in"].as_i64().unwrap(),
                src_out: s["src_out"].as_i64().unwrap(),
                timeline_start: s["timeline_start"].as_i64().unwrap(),
                fade: match t["type"].as_str().unwrap() {
                    "cut" => 0,
                    "crossfade" => t["duration"].as_i64().unwrap(),
                    other => panic!("unknown transition {other}"),
                },
                speed: s["speed"].as_i64().unwrap(),
            }
        })
        .collect()
}

/// What the frame shown at output time `t` must be: `Some((clip, source
/// frame))`, or `None` inside a crossfade (or past the last segment).
fn expected_frame(segs: &[Seg], t: i64) -> Option<(usize, i64)> {
    let (i, seg) = segs
        .iter()
        .enumerate()
        .rev()
        .find(|(_, s)| s.timeline_start <= t)?;
    let in_fade = |s: &Seg| s.fade > 0 && t >= s.timeline_start && t < s.timeline_start + s.fade;
    if in_fade(seg) || t >= seg.timeline_end() {
        return None;
    }
    // A crossfade into the next segment overlaps the end of this one.
    if segs.get(i + 1).is_some_and(in_fade) {
        return None;
    }
    let src = seg.src_in + (t - seg.timeline_start) * seg.speed / 100;
    // The renderer shows the source frame displayed at `src`.
    Some((seg.clip, src * FPS / 1_000_000))
}

/// Times (µs) where the mono `samples` at `rate` Hz rise out of silence:
/// the first sample of each burst whose RMS over 1 ms windows exceeds
/// `threshold`, with bursts at least 100 ms apart.
fn audio_onsets(samples: &[f32], rate: u32, threshold: f32) -> Vec<i64> {
    let win = (rate / 1000).max(1) as usize;
    let gap = rate as usize / 10;
    let mut onsets = Vec::new();
    let mut last: Option<usize> = None;
    for (w, chunk) in samples.chunks(win).enumerate() {
        let rms = (chunk.iter().map(|x| x * x).sum::<f32>() / chunk.len() as f32).sqrt();
        let start = w * win;
        if rms >= threshold && last.is_none_or(|l| start - l >= gap) {
            // Refine to the first sample above the threshold in this window.
            let first = chunk.iter().position(|x| x.abs() >= threshold).unwrap_or(0);
            onsets.push(((start + first) as i64) * 1_000_000 / i64::from(rate));
        }
        if rms >= threshold {
            last = Some(start);
        }
    }
    onsets
}

/// Decode the first channel of the audio track of `path` with FFmpeg, with
/// the AAC encoder delay trimmed as the MP4 edit list says (players do this;
/// symphonia, used by `kv-audio`, does not, which shifts everything ~21 ms).
fn decode_left_channel(path: &Path) -> (Vec<f32>, u32) {
    ff::init().unwrap();
    let mut input = ff::format::input(path).unwrap();
    let stream = input.streams().best(ff::media::Type::Audio).unwrap();
    let index = stream.index();
    let mut decoder = ff::codec::context::Context::from_parameters(stream.parameters())
        .and_then(|c| c.decoder().audio())
        .unwrap();
    let mut samples = Vec::new();
    let mut frame = ff::frame::Audio::empty();
    let mut drain = |decoder: &mut ff::decoder::Audio, samples: &mut Vec<f32>| {
        while decoder.receive_frame(&mut frame).is_ok() {
            assert_eq!(
                frame.format(),
                ff::format::Sample::F32(ff::format::sample::Type::Planar),
                "AAC decodes to planar f32"
            );
            samples.extend_from_slice(&frame.plane::<f32>(0)[..frame.samples()]);
        }
    };
    for (s, packet) in input.packets() {
        if s.index() == index {
            decoder.send_packet(&packet).unwrap();
            drain(&mut decoder, &mut samples);
        }
    }
    decoder.send_eof().unwrap();
    drain(&mut decoder, &mut samples);
    (samples, decoder.rate())
}

fn nearest(times: &[i64], t: i64) -> i64 {
    times.iter().map(|&b| (b - t).abs()).min().unwrap()
}

#[test]
fn ten_clips_and_a_song_make_a_beat_synced_mp4() {
    let started = Instant::now();
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::create_dir(root.join("clips")).unwrap();
    for src in tagged_clip_set(CLIPS, CLIP_SECS) {
        fs::copy(&src, root.join("clips").join(src.file_name().unwrap())).unwrap();
    }
    fs::copy(
        click_track(BPM, SONG_SECS, DEFAULT_SAMPLE_RATE),
        root.join("song.wav"),
    )
    .unwrap();

    // Determinism: the same seed gives the same edit, byte for byte.
    let plan = dry_run_json(root);
    assert_eq!(plan, dry_run_json(root), "dry run is not deterministic");
    let edl: Value = serde_json::from_str(&plan).unwrap();
    let paths: Vec<&str> = edl["project"]["media"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["path"].as_str().unwrap())
        .collect();
    let expected_paths: Vec<String> = (1..=CLIPS)
        .map(|i| format!("clips/GX01{i:04}.MP4"))
        .collect();
    assert_eq!(paths, expected_paths, "media in recording order");
    let segs = segments(&edl);
    let edl_len = edl["total_length"].as_i64().unwrap();

    // Render.
    let render_started = Instant::now();
    let out = edit(root, &["--size", "320x180", "--out", "out.mp4"])
        .assert()
        .success()
        .get_output()
        .clone();
    let render_secs = render_started.elapsed().as_secs_f64();
    let stderr = String::from_utf8(out.stderr).unwrap();
    let summary = stderr.lines().last().unwrap_or_default();
    let encoder = summary
        .split_once(" encoder ")
        .and_then(|(_, r)| r.split(',').next())
        .unwrap_or_else(|| panic!("no encoder in summary: {summary}"));
    eprintln!("m1 gate: encoder {encoder}, render {render_secs:.1} s");

    // Container and streams.
    let backend = FfmpegBackend::new().unwrap();
    let mp4 = root.join("out.mp4");
    let info = backend.probe(&mp4).unwrap();
    assert!(
        info.container.contains("mp4"),
        "container {}",
        info.container
    );
    let video = info.video.clone().expect("video stream");
    let audio = info.audio.clone().expect("audio stream");
    assert_eq!(video.codec, "h264");
    assert_eq!((video.width, video.height), (320, 180));
    assert_eq!(audio.codec, "aac");
    assert_eq!(audio.channels, 2, "stereo audio");
    assert!(
        (info.duration.0 - LENGTH_US).abs() <= BEAT_US,
        "duration {:?} not within a beat of 15 s",
        info.duration
    );
    assert!(
        (info.duration.0 - edl_len).abs() <= FRAME_US,
        "duration {:?} != EDL length {edl_len}",
        info.duration
    );

    // Every frame: tag present outside crossfades and equal to the EDL.
    let mut reader = backend.open_video(&mp4, &DecodeOptions::default()).unwrap();
    let mut frames = Vec::new(); // (pts, tag) per output frame
    while let Some(frame) = reader.next_frame().unwrap() {
        assert_eq!(frame.format, PixelFormat::Rgba8);
        let tag = read_frame_tag(&frame.data, frame.width, frame.height);
        let t = frame.pts.0;
        match expected_frame(&segs, t) {
            Some((clip, src_frame)) => {
                let tag = tag.unwrap_or_else(|| panic!("frame at {t} µs has no tag"));
                assert_eq!(usize::from(tag.clip), clip, "clip of frame at {t} µs");
                assert!(
                    (i64::from(tag.frame) - src_frame).abs() <= 1,
                    "frame at {t} µs shows source frame {}, EDL says {src_frame}",
                    tag.frame
                );
            }
            None => assert!(
                t >= edl_len - FRAME_US || segs.iter().any(|s| s.fade > 0),
                "frame at {t} µs is outside every segment"
            ),
        }
        frames.push((t, tag));
    }
    let expected_frames = (edl_len + FRAME_US - 1) / FRAME_US;
    assert!(
        (frames.len() as i64 - expected_frames).abs() <= 1,
        "{} frames, expected about {expected_frames}",
        frames.len()
    );

    // Cuts, read from the pictures alone, land on the beat.
    let beats: Vec<i64> = beat_times(BPM, SONG_SECS)
        .iter()
        .map(|b| (b * 1e6).round() as i64)
        .collect();
    let mut cuts = Vec::new();
    for pair in frames.windows(2) {
        let ((_, Some(a)), (t, Some(b))) = (pair[0], pair[1]) else {
            continue;
        };
        if a.clip != b.clip || i64::from(b.frame) - i64::from(a.frame) != 1 {
            cuts.push(t);
        }
    }
    let edl_cuts = segs.len() - 1;
    assert!(
        cuts.len() >= edl_cuts.min(5),
        "only {} cuts detected for {} segments: {cuts:?}",
        cuts.len(),
        segs.len()
    );
    for &cut in &cuts {
        assert!(
            nearest(&beats, cut) <= FRAME_US,
            "cut at {cut} µs is {} µs from the nearest beat",
            nearest(&beats, cut)
        );
    }

    // Spread across the shoot.
    let mut clips: Vec<u8> = frames
        .iter()
        .filter_map(|(_, tag)| tag.map(|t| t.clip))
        .collect();
    clips.sort_unstable();
    clips.dedup();
    assert!(clips.len() >= 5, "only clips {clips:?} appear");

    // The music in the file is in sync: every click is audible within
    // 20 ms of its time in the song (the edit starts at song time 0).
    let (left, rate) = decode_left_channel(&mp4);
    assert_eq!(rate, audio.sample_rate);
    let onsets = audio_onsets(&left, rate, 0.05);
    // Clicks before the export's 1 s fade-out gets too quiet to hear.
    let audible: Vec<i64> = beats
        .iter()
        .copied()
        .filter(|&b| b < info.duration.0 - 250_000)
        .collect();
    assert!(
        onsets.len() >= audible.len(),
        "{} onsets for {} clicks: {onsets:?}",
        onsets.len(),
        audible.len()
    );
    for &onset in &onsets {
        assert!(
            nearest(&beats, onset) <= AUDIO_SYNC_US,
            "audio onset at {onset} µs is {} µs from the nearest click",
            nearest(&beats, onset)
        );
    }
    for &click in &audible {
        assert!(
            nearest(&onsets, click) <= AUDIO_SYNC_US,
            "click at {click} µs is not heard in the output"
        );
    }

    eprintln!(
        "m1 gate: {} segments, {} cuts on the beat, {} clips, {} frames, {:.1} s total",
        segs.len(),
        cuts.len(),
        clips.len(),
        frames.len(),
        started.elapsed().as_secs_f64()
    );
}

/// Corpus files for the real-footage variant: the first 10 manifest entries
/// whose name starts with this, in name order (#106).
const CORPUS_PREFIX: &str = "m1-gate-";

/// The same command on 10 real GoPro clips from the regression corpus, given
/// as one clip directory: exit 0, duration 15 s +- one beat, and every segment
/// boundary within one frame of a beat of the song as `kv_audio::analyze`
/// hears it. The song is the click track, so no licensed music is needed.
/// Skips until the owner adds the clips (#106).
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
        let require = std::env::var("KV_REQUIRE_CORPUS").is_ok_and(|v| v == "1");
        let reason = format!(
            "corpus has {} of {CLIPS} `{CORPUS_PREFIX}*` clips (#106)",
            names.len()
        );
        assert!(!require, "{reason} (KV_REQUIRE_CORPUS=1)");
        eprintln!("SKIPPED: {reason}");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::create_dir(root.join("clips")).unwrap();
    for name in &names[..CLIPS] {
        let path = kv_testkit::corpus_file_or_skip!(name);
        fs::copy(&path, root.join("clips").join(name)).unwrap();
    }
    let song = root.join("song.wav");
    fs::copy(click_track(BPM, SONG_SECS, DEFAULT_SAMPLE_RATE), &song).unwrap();

    let edl: Value = serde_json::from_str(&dry_run_json(root)).unwrap();
    let segs = segments(&edl);
    let started = Instant::now();
    edit(root, &["--out", "out.mp4"]).assert().success();
    eprintln!(
        "m1 gate (corpus): {} segments, render {:.1} s",
        segs.len(),
        started.elapsed().as_secs_f64()
    );

    let info = FfmpegBackend::new()
        .unwrap()
        .probe(&root.join("out.mp4"))
        .unwrap();
    assert!(
        (info.duration.0 - LENGTH_US).abs() <= BEAT_US,
        "duration {:?} not within a beat of 15 s",
        info.duration
    );

    let beats: Vec<i64> = kv_audio::analyze(&song)
        .unwrap()
        .beats
        .iter()
        .map(|b| b.0)
        .collect();
    for seg in &segs[1..] {
        // A crossfade is centred on its beat (guess); a cut has fade 0.
        let cut = seg.timeline_start + seg.fade / 2;
        assert!(
            nearest(&beats, cut) <= FRAME_US,
            "segment boundary at {cut} µs is {} µs from the nearest beat",
            nearest(&beats, cut)
        );
    }
}
