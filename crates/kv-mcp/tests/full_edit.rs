//! The Roadmap I gate (`docs/roadmap.md`): "MCP drives a full edit".
//!
//! [`mcp_drives_a_full_edit`] spawns the real `kwikvid-mcp` binary and, over
//! stdio like Claude Desktop or Claude Code would (`initialize`, then
//! `tools/call` only), takes an empty project to an exported MP4 and a saved,
//! reopened project: import (with a chaptered GoPro recording), music, target
//! length, a pin and an exclude, a re-roll, style, aspect and title, manual
//! tweaks, undo and redo, save / new / open, export. It then decodes the MP4
//! and checks it frame by frame against the final `project_state` EDL, as
//! `crates/kv-cli/tests/m1_gate.rs` does for the CLI. It runs in the merge
//! gate on Linux and in `mac-smoke` on macOS.

mod common;

use std::collections::HashMap;
use std::fs;
use std::time::Instant;

use common::Client;
use kv_media::{DecodeOptions, FfmpegBackend, MediaBackend, PixelFormat};
use kv_testkit::{
    beat_times, click_track, read_frame_tag, render_tagged_frame, tagged_clip_set, FrameTag,
    DEFAULT_SAMPLE_RATE,
};
use serde_json::{json, Value};

/// Tagged clips generated: the first six are single files, the last two the
/// chapters of one recording.
const CLIPS: usize = 8;
const CLIP_SECS: f64 = 4.0;
const CLIP_US: i64 = 4_000_000;
const BPM: f64 = 120.0;
const SONG_SECS: f64 = 30.0;
const LENGTH_US: i64 = 15_000_000;
const FPS: i64 = 30;
/// One output frame at 30 fps, in microseconds.
const FRAME_US: i64 = 1_000_000 / FPS;
/// One beat at 120 BPM.
const BEAT_US: i64 = 500_000;
/// Square 1080p.
const SIDE: u32 = 1080;
const TITLE: &str = "MCP test";

/// One segment of the EDL, times in microseconds.
#[derive(Debug, Clone, Copy)]
struct Seg {
    media_id: u64,
    src_in: i64,
    src_out: i64,
    timeline_start: i64,
    /// Crossfade overlap with the previous segment (0 for a cut).
    fade: i64,
    /// Percent (100 = normal).
    speed: i64,
}

impl Seg {
    fn timeline_end(&self) -> i64 {
        self.timeline_start + (self.src_out - self.src_in) * 100 / self.speed
    }
}

fn segments(project: &Value) -> Vec<Seg> {
    project["sections"][0]["edit"]["segments"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| {
            let t = &s["transition"];
            Seg {
                media_id: s["media_id"].as_u64().unwrap(),
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

/// The files of each media item: `(clip index, duration)` per file, in
/// order (one entry, or one per chapter), keyed by media id.
fn media_files(project: &Value, clip_of: &HashMap<String, u8>) -> HashMap<u64, Vec<(u8, i64)>> {
    let clip = |path: &Value| clip_of[path.as_str().unwrap().rsplit('/').next().unwrap()];
    project["media"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| {
            let files = match m["chapters"].as_array() {
                Some(ch) => ch
                    .iter()
                    .map(|c| (clip(&c["path"]), c["duration"].as_i64().unwrap()))
                    .collect(),
                None => vec![(clip(&m["path"]), m["duration"].as_i64().unwrap())],
            };
            (m["id"].as_u64().unwrap(), files)
        })
        .collect()
}

/// What the frame at output time `t` must show: `Some(tag)`, or `None` in a
/// crossfade (or past the last segment).
fn expected_tag(segs: &[Seg], files: &HashMap<u64, Vec<(u8, i64)>>, t: i64) -> Option<FrameTag> {
    let (i, seg) = segs
        .iter()
        .enumerate()
        .rev()
        .find(|(_, s)| s.timeline_start <= t)?;
    let in_fade = |s: &Seg| s.fade > 0 && t >= s.timeline_start && t < s.timeline_start + s.fade;
    if in_fade(seg) || t >= seg.timeline_end() || segs.get(i + 1).is_some_and(in_fade) {
        return None;
    }
    let mut src = seg.src_in + (t - seg.timeline_start) * seg.speed / 100;
    // Item time runs on across chapters.
    for &(clip, duration) in &files[&seg.media_id] {
        if src < duration {
            return Some(FrameTag {
                clip,
                frame: u16::try_from(src * FPS / 1_000_000).unwrap(),
            });
        }
        src -= duration;
    }
    panic!("segment {seg:?} runs past the end of its media");
}

fn nearest(times: &[i64], t: i64) -> i64 {
    times.iter().map(|&b| (b - t).abs()).min().unwrap()
}

/// Mean absolute RGB difference between two RGBA frames over the rows
/// `rows` (the band the title is drawn in).
fn band_diff(a: &[u8], b: &[u8], width: u32, rows: std::ops::Range<u32>) -> f64 {
    let stride = width as usize * 4;
    let (mut sum, mut n) = (0u64, 0u64);
    for y in rows {
        let row = y as usize * stride;
        for px in (row..row + stride).step_by(4) {
            for c in 0..3 {
                sum += u64::from(a[px + c].abs_diff(b[px + c]));
                n += 1;
            }
        }
    }
    sum as f64 / n as f64
}

/// Whether the union of the segments of `media_id` covers `[start, end)`.
fn covered(segs: &[Seg], media_id: u64, start: i64, end: i64) -> bool {
    let mut ranges: Vec<(i64, i64)> = segs
        .iter()
        .filter(|s| s.media_id == media_id)
        .map(|s| (s.src_in, s.src_out))
        .collect();
    ranges.sort_unstable();
    let mut reach = start;
    for (a, b) in ranges {
        if a <= reach {
            reach = reach.max(b);
        }
    }
    reach >= end
}

#[test]
fn mcp_drives_a_full_edit() {
    let started = Instant::now();
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::create_dir(root.join("clips")).unwrap();
    // File name -> clip index (the tag in every frame of that file).
    let mut clip_of = HashMap::new();
    for (i, src) in tagged_clip_set(CLIPS, CLIP_SECS).iter().enumerate() {
        let name = match i {
            6 => "GX010042.MP4".to_owned(),
            7 => "GX020042.MP4".to_owned(),
            _ => src.file_name().unwrap().to_str().unwrap().to_owned(),
        };
        fs::copy(src, root.join("clips").join(&name)).unwrap();
        clip_of.insert(name, i as u8);
    }
    fs::copy(
        click_track(BPM, SONG_SECS, DEFAULT_SAMPLE_RATE),
        root.join("song.wav"),
    )
    .unwrap();
    let beats: Vec<i64> = beat_times(BPM, SONG_SECS)
        .iter()
        .map(|b| (b * 1e6).round() as i64)
        .collect();

    let mut c = Client::spawn_in(root, &root.join(".cache"));
    c.initialize();
    let state = |c: &mut Client| c.call_ok("project_state", json!({}));

    // 1. Import the folder: the two chapters become one item.
    let r = c.call_ok("media_import", json!({"paths": ["clips"]}));
    assert_eq!(r["added"], json!([1, 2, 3, 4, 5, 6, 7]), "{r}");
    assert_eq!(r["skipped"], json!([]), "{r}");
    let project = state(&mut c)["project"].clone();
    let media = project["media"].as_array().unwrap();
    assert_eq!(media.len(), CLIPS - 1, "{project}");
    let chaptered: Vec<&Value> = media
        .iter()
        .filter(|m| m["chapters"].as_array().is_some_and(|c| !c.is_empty()))
        .collect();
    assert_eq!(chaptered.len(), 1, "{project}");
    assert_eq!(chaptered[0]["path"], "clips/GX010042.MP4");
    assert_eq!(chaptered[0]["chapters"][1]["path"], "clips/GX020042.MP4");
    let d = chaptered[0]["duration"].as_i64().unwrap();
    assert!(
        (d - 2 * CLIP_US).abs() <= 2 * FRAME_US,
        "chaptered duration {d}"
    );

    // 2. Music and a 15 s target.
    let r = c.call_ok("music_set", json!({"path": "song.wav"}));
    assert!((r["tempo_bpm"].as_f64().unwrap() - BPM).abs() < 2.0, "{r}");
    let r = c.call_ok("edit_set_target_length", json!({"seconds": 15}));
    assert_eq!(r["regen_error"], Value::Null, "{r}");

    // 3. Pin 1.5 s of clip 2, exclude all of clip 4.
    const PINNED: u64 = 2;
    const EXCLUDED: u64 = 4;
    let (pin_in, pin_out) = (500_000, 2_000_000);
    let r = c.call_ok(
        "edit_pin_range",
        json!({"media_id": PINNED, "start_us": pin_in, "end_us": pin_out}),
    );
    assert_eq!(r["regen_error"], Value::Null, "{r}");
    let r = c.call_ok(
        "edit_exclude_range",
        json!({"media_id": EXCLUDED, "start_us": 0, "end_us": CLIP_US}),
    );
    assert_eq!(r["regen_error"], Value::Null, "{r}");

    // The one deliberate bad call: a pin inside the exclude is refused with
    // a readable message and changes nothing.
    let before = state(&mut c);
    let (is_error, text) = c.call(
        "edit_pin_range",
        json!({"media_id": EXCLUDED, "start_us": 1_000_000, "end_us": 1_500_000}),
    );
    assert!(is_error, "pin over an exclude was accepted: {text}");
    assert!(
        text.len() > 10 && text.chars().any(char::is_alphabetic),
        "unreadable error: {text:?}"
    );
    assert_eq!(state(&mut c), before, "a refused pin changed the project");

    // 4. Re-roll: the pin stays, the excluded clip stays out.
    let seed = before["project"]["sections"][0]["seed"].clone();
    let r = c.call_ok("edit_reroll", json!({}));
    assert_eq!(r["regen_error"], Value::Null, "{r}");
    let project = state(&mut c)["project"].clone();
    assert_ne!(
        project["sections"][0]["seed"], seed,
        "re-roll kept the seed"
    );
    let segs = segments(&project);
    assert!(
        covered(&segs, PINNED, pin_in, pin_out),
        "pin lost: {segs:?}"
    );
    assert!(segs.iter().all(|s| s.media_id != EXCLUDED), "{segs:?}");

    // 5. Look: style (rebuilds the edit), square, title. Done before the
    // manual tweaks, which a rebuild would replace.
    let r = c.call_ok("style_set", json!({"style": "smooth"}));
    assert_eq!(r["regen_error"], Value::Null, "{r}");
    c.call_ok("aspect_set", json!({"aspect": "square1x1"}));
    c.call_ok("title_set", json!({"text": TITLE}));

    // 6. Manual tweaks: none of them rebuilds the edit.
    // Swap segments 1 and 2. A move repacks the timeline and every segment
    // keeps its own transition, so the cuts stay on the beat only when the
    // crossfades around the moved segments are equally long.
    let segs = segments(&state(&mut c)["project"]);
    assert!(segs.len() >= 4, "only {} segments", segs.len());
    assert!(
        segs[1..4]
            .iter()
            .all(|s| (s.fade - segs[1].fade).abs() <= FRAME_US / 4),
        "crossfades around segments 1 and 2 differ: {segs:?}"
    );
    let r = c.call_ok("edit_move_segment", json!({"from": 1, "to": 2}));
    assert_eq!(r["manual_edits_replaced"], false, "{r}");
    // A cut that can move a beat later: the clip before it has a beat (and
    // its fade) of footage left, the segment after it is over two beats.
    let project = state(&mut c)["project"].clone();
    let durations: HashMap<u64, i64> = project["media"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| (m["id"].as_u64().unwrap(), m["duration"].as_i64().unwrap()))
        .collect();
    let segs = segments(&project);
    let cut = (1..segs.len())
        .find(|&k| {
            let (a, b) = (segs[k - 1], segs[k]);
            a.speed == 100
                && b.speed == 100
                && a.src_out + BEAT_US + b.fade + FRAME_US <= durations[&a.media_id]
                && b.timeline_end() - b.timeline_start > 2 * BEAT_US + b.fade
        })
        .unwrap_or_else(|| panic!("no cut can move a beat later: {segs:?}"));
    let r = c.call_ok("edit_nudge_cut", json!({"cut": cut, "beats": 1}));
    assert_eq!(r["manual_edits_replaced"], false, "{r}");
    let centre = |s: &Seg| s.timeline_start + s.fade / 2;
    let moved = centre(&segments(&state(&mut c)["project"])[cut]) - centre(&segs[cut]);
    assert!(
        (moved - BEAT_US).abs() <= FRAME_US,
        "cut {cut} moved {moved} µs"
    );
    c.call_ok("clip_audio_set", json!({"media_id": 1, "enabled": true}));
    let before_undo = state(&mut c)["project"].clone();
    c.call_ok("music_volume_set", json!({"percent": 80}));

    // 7. Undo and redo the last step.
    let done = state(&mut c);
    let r = c.call_ok("edit_undo", json!({}));
    assert_eq!(r["can_redo"], true, "{r}");
    assert_eq!(state(&mut c)["project"], before_undo, "undo");
    c.call_ok("edit_redo", json!({}));
    let redone = state(&mut c);
    assert_eq!(redone["project"], done["project"], "redo");
    assert_eq!(redone["can_redo"], false);

    // 8. Save, new, open: the same project.
    let r = c.call_ok("project_save_as", json!({"path": "full-edit"}));
    assert_eq!(r["file"], "full-edit.kwikvid", "{r}");
    let saved = state(&mut c)["project"].clone();
    let r = c.call_ok("project_new", json!({}));
    assert_eq!(r["edit"]["segments"], 0, "{r}");
    let r = c.call_ok("project_open", json!({"path": "full-edit.kwikvid"}));
    assert_eq!(r["missing"], json!([]), "{r}");
    assert!(r.get("analysis_errors").is_none(), "{r}");
    let project = state(&mut c)["project"].clone();
    assert_eq!(project, saved, "reopened project differs");
    let section = &project["sections"][0];
    assert_eq!(section["style"], "smooth", "{section}");
    assert_eq!(section["title"]["text"], TITLE, "{section}");
    assert_eq!(project["settings"]["music_volume_percent"], 80, "{project}");
    assert_eq!(
        project["settings"]["aspect_ratio"], "square1x1",
        "{project}"
    );

    // 9. Export square 1080p and wait for it.
    let export_started = Instant::now();
    let started_export = c.call_ok(
        "export_start",
        json!({"path": "out.mp4", "resolution": "1080p"}),
    );
    assert_eq!(started_export["width"], SIDE, "{started_export}");
    assert_eq!(started_export["height"], SIDE, "{started_export}");
    let id = started_export["id"].as_u64().unwrap();
    let mut status = Value::Null;
    for _ in 0..20 {
        status = c.call_ok("export_status", json!({"id": id, "wait_ms": 30_000}));
        if status["state"] != "started" && status["state"] != "progress" {
            break;
        }
    }
    assert_eq!(status["state"], "done", "{status}");
    let export_secs = export_started.elapsed().as_secs_f64();
    drop(c);

    // The file against the final EDL.
    let segs = segments(&project);
    let files = media_files(&project, &clip_of);
    let edl_len = segs.iter().map(Seg::timeline_end).max().unwrap();
    assert!(
        covered(&segs, PINNED, pin_in, pin_out),
        "pin lost: {segs:?}"
    );
    assert!(segs.iter().all(|s| s.media_id != EXCLUDED), "{segs:?}");
    assert!(segs.iter().any(|s| s.fade > 0), "smooth has no crossfades");
    for s in &segs[1..] {
        // A crossfade is centred on its beat (the auto-edit's rule); a cut
        // has fade 0.
        let cut = s.timeline_start + s.fade / 2;
        assert!(
            nearest(&beats, cut) <= FRAME_US,
            "boundary at {cut} µs is {} µs off the beat",
            nearest(&beats, cut)
        );
    }
    let title_us = project["sections"][0]["title"]["duration"]
        .as_i64()
        .unwrap();

    let backend = FfmpegBackend::new().unwrap();
    let mp4 = root.join("out.mp4");
    let info = backend.probe(&mp4).unwrap();
    let video = info.video.clone().expect("video stream");
    assert_eq!((video.width, video.height), (SIDE, SIDE));
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

    // Every frame after the title shows what the EDL says; in the title the
    // centre band differs from the untitled frame, and after it does not.
    // The text is 8 % of the height, centred.
    let band = SIDE / 2 - SIDE / 24..SIDE / 2 + SIDE / 24;
    let (mut titled, mut untitled) = (Vec::new(), Vec::new());
    let mut frames = Vec::new();
    let mut reader = backend.open_video(&mp4, &DecodeOptions::default()).unwrap();
    while let Some(frame) = reader.next_frame().unwrap() {
        assert_eq!(frame.format, PixelFormat::Rgba8);
        let t = frame.pts.0;
        let tag = read_frame_tag(&frame.data, frame.width, frame.height);
        let expected = expected_tag(&segs, &files, t);
        let sample = (t / FRAME_US) % 10 == 5;
        if t < title_us {
            if let (Some(want), true) = (expected, sample && t > 500_000 && t < title_us - 500_000)
            {
                let plain = render_tagged_frame(want, SIDE, SIDE);
                titled.push(band_diff(&frame.data, &plain, SIDE, band.clone()));
            }
        } else if let Some(want) = expected {
            let got = tag.unwrap_or_else(|| panic!("frame at {t} µs has no tag"));
            assert_eq!(got.clip, want.clip, "clip of frame at {t} µs");
            assert!(
                (i64::from(got.frame) - i64::from(want.frame)).abs() <= 1,
                "frame at {t} µs shows source frame {}, EDL says {}",
                got.frame,
                want.frame
            );
            if sample {
                let plain = render_tagged_frame(want, SIDE, SIDE);
                untitled.push(band_diff(&frame.data, &plain, SIDE, band.clone()));
            }
        }
        frames.push((t, tag));
    }
    let expected_frames = (edl_len + FRAME_US - 1) / FRAME_US;
    assert!(
        (frames.len() as i64 - expected_frames).abs() <= 1,
        "{} frames, expected about {expected_frames}",
        frames.len()
    );
    let worst_untitled = untitled.iter().copied().fold(0.0, f64::max);
    let least_titled = titled.iter().copied().fold(f64::MAX, f64::min);
    assert!(!titled.is_empty() && !untitled.is_empty());
    assert!(
        least_titled > 2.0 * worst_untitled.max(2.0),
        "title not visible: titled band diffs {titled:?}, untitled {untitled:?}"
    );

    // Transitions, read from the pictures alone, sit on the beat: where the
    // tag jumps (a cut), or the middle of the untagged run where two tags
    // blend (a crossfade), after the title.
    let tagged: Vec<(i64, FrameTag)> = frames
        .iter()
        .filter_map(|&(t, tag)| tag.map(|tag| (t, tag)))
        .filter(|&(t, _)| t >= title_us)
        .collect();
    let mut cuts = Vec::new();
    for pair in tagged.windows(2) {
        let ((ta, a), (tb, b)) = (pair[0], pair[1]);
        if a.clip != b.clip || i64::from(b.frame) - i64::from(a.frame) != (tb - ta) / FRAME_US {
            cuts.push((ta + FRAME_US + tb) / 2);
        }
    }
    let edl_cuts = segs[1..]
        .iter()
        .filter(|s| s.timeline_start > title_us)
        .count();
    assert_eq!(
        cuts.len(),
        edl_cuts,
        "transitions seen at {cuts:?}, EDL {segs:?}"
    );
    for &cut in &cuts {
        assert!(
            nearest(&beats, cut) <= FRAME_US,
            "transition at {cut} µs is {} µs from the nearest beat",
            nearest(&beats, cut)
        );
    }

    eprintln!(
        "roadmap I gate: {} segments ({} transitions seen on the beat), {} frames, export {export_secs:.1} s, \
         {:.1} s total",
        segs.len(),
        cuts.len(),
        frames.len(),
        started.elapsed().as_secs_f64()
    );
}
