//! End-to-end tests of the `kwikvid` binary on kv-testkit fixtures.
//!
//! Each test runs the binary inside a temporary directory holding a click
//! track (`song.wav`) and empty clip files (`clips/*.mp4`); clip durations come
//! from `--durations`, so no real video is needed. Paths are relative to that
//! directory, which keeps the JSON snapshots machine independent. The render
//! tests copy real tagged clips (`kv_testkit::tagged_clip_set`) into it.

use std::fs;
use std::path::Path;

use assert_cmd::cargo::cargo_bin_cmd;
use assert_cmd::Command;
use kv_media::{FfmpegBackend, MediaBackend};
use kv_testkit::{click_track, tagged_clip_set, DEFAULT_SAMPLE_RATE};
use serde_json::Value;
use tempfile::TempDir;

const BPM: f64 = 120.0;
const SONG_SECS: f64 = 16.0;

/// A temp dir with `song.wav` (120 BPM clicks, 16 s) and `clips/{a,b,c}.mp4`.
fn workspace() -> TempDir {
    let dir = tempfile::tempdir().unwrap();
    fs::copy(
        click_track(BPM, SONG_SECS, DEFAULT_SAMPLE_RATE),
        dir.path().join("song.wav"),
    )
    .unwrap();
    fs::create_dir(dir.path().join("clips")).unwrap();
    for name in ["a.mp4", "b.mp4", "c.mp4"] {
        fs::write(dir.path().join("clips").join(name), b"").unwrap();
    }
    dir
}

fn kwikvid(dir: &Path) -> Command {
    let mut cmd = cargo_bin_cmd!("kwikvid");
    cmd.current_dir(dir);
    cmd
}

const DURATIONS: &str = "a.mp4=8,b.mp4=6.5,c.mp4=12";

/// `edit --dry-run` over the three clips with extra arguments.
fn edit(dir: &Path, extra: &[&str]) -> Command {
    let mut cmd = kwikvid(dir);
    cmd.args([
        "edit",
        "--clips",
        "clips/a.mp4",
        "clips/b.mp4",
        "clips/c.mp4",
        "--music",
        "song.wav",
        "--durations",
        DURATIONS,
        "--dry-run",
    ])
    .args(extra);
    cmd
}

fn stdout(cmd: &mut Command) -> String {
    let out = cmd.assert().success().get_output().stdout.clone();
    String::from_utf8(out).unwrap()
}

fn stderr_of_failure(cmd: &mut Command) -> String {
    let out = cmd.assert().failure().code(1).get_output().stderr.clone();
    String::from_utf8(out).unwrap()
}

fn json(cmd: &mut Command) -> Value {
    serde_json::from_str(&stdout(cmd)).unwrap()
}

/// `(media_id, src_in, src_out)` of every segment in an edit JSON.
fn segments(v: &Value) -> Vec<(u64, i64, i64)> {
    v["project"]["sections"][0]["edit"]["segments"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| {
            (
                s["media_id"].as_u64().unwrap(),
                s["src_in"].as_i64().unwrap(),
                s["src_out"].as_i64().unwrap(),
            )
        })
        .collect()
}

#[test]
fn analyze_music_text() {
    let dir = workspace();
    let out = stdout(kwikvid(dir.path()).args(["analyze-music", "song.wav"]));
    assert!(out.starts_with("song.wav\n"), "{out}");
    let tempo: f64 = out
        .lines()
        .find_map(|l| l.trim().strip_prefix("tempo"))
        .and_then(|t| t.trim().strip_suffix(" BPM"))
        .unwrap()
        .parse()
        .unwrap();
    assert!((tempo - BPM).abs() < 0.5, "{out}");
    assert!(out.contains("beats     32 (8 downbeats)"), "{out}");
    assert!(out.contains("first beats (s): 0.000 "), "{out}");
}

#[test]
fn analyze_music_json() {
    let dir = workspace();
    let out =
        stdout(kwikvid(dir.path()).args(["analyze-music", "song.wav", "--json", "--first", "4"]));
    insta::assert_snapshot!(out);
}

#[test]
fn edit_dry_run_json() {
    let dir = workspace();
    let out = stdout(&mut edit(
        dir.path(),
        &[
            "--length",
            "10s",
            "--seed",
            "3",
            "--pin",
            "b.mp4@2-3.5",
            "--exclude",
            "clips/c.mp4@0-4",
            "--json",
        ],
    ));
    insta::assert_snapshot!(out);
}

#[test]
fn edit_dry_run_table() {
    let dir = workspace();
    let out = stdout(&mut edit(
        dir.path(),
        &["--length", "10s", "--pin", "a.mp4@1-2"],
    ));
    assert!(
        out.contains("music   song.wav (") && out.contains(" BPM, 32 beats, 16.000 s)"),
        "{out}"
    );
    assert!(out.contains("target  10.000 s, seed 0"), "{out}");
    assert!(out.contains("clips   3, 1 pinned, 0 excluded"), "{out}");
    assert!(out.contains("#  clip"), "{out}");
    assert!(out.contains("clips/a.mp4"), "{out}");
    assert!(out.contains("  pinned\n"), "{out}");
    assert!(out.contains("\ntotal   "), "{out}");
}

#[test]
fn edit_honours_pins_excludes_and_length() {
    let dir = workspace();
    let v = json(&mut edit(
        dir.path(),
        &[
            "--length",
            "12",
            "--pin",
            "clips/a.mp4@5-6.5",
            "--exclude",
            "b.mp4",
            "--json",
        ],
    ));
    assert_eq!(v["target_length"], 12_000_000);
    // The edit ends on the beat closest to the target (beats are 0.5 s apart).
    let total = v["total_length"].as_i64().unwrap();
    assert!((total - 12_000_000).abs() <= 250_000, "{total}");
    let segs = segments(&v);
    assert!(segs
        .iter()
        .any(|&(m, a, b)| m == 1 && a <= 5_000_000 && b >= 6_500_000));
    assert!(segs.iter().all(|&(m, _, _)| m != 2), "b.mp4 is excluded");
    let media = &v["project"]["media"];
    assert_eq!(media[0]["path"], "clips/a.mp4");
    assert_eq!(media[1]["duration"], 6_500_000);
    assert_eq!(v["project"]["sections"][0]["music"]["path"], "song.wav");
}

#[test]
fn edit_full_song_is_the_default_length() {
    let dir = workspace();
    let v = json(&mut edit(dir.path(), &["--json"]));
    assert_eq!(v["target_length"], v["music"]["duration"]);
    assert_eq!(
        v["project"]["sections"][0]["target_length"]["type"],
        "full_song"
    );
}

#[test]
fn edit_is_deterministic_per_seed() {
    let dir = workspace();
    let run = |seed: &str| {
        stdout(&mut edit(
            dir.path(),
            &["--length", "10s", "--seed", seed, "--json"],
        ))
    };
    assert_eq!(run("7"), run("7"));
    let v: Value = serde_json::from_str(&run("7")).unwrap();
    assert_eq!(v["project"]["sections"][0]["seed"], 7);
}

#[test]
fn edit_accepts_a_clip_directory() {
    let dir = workspace();
    fs::write(dir.path().join("clips/notes.txt"), b"not a clip").unwrap();
    let v = json(kwikvid(dir.path()).args([
        "edit",
        "--clips",
        "clips",
        "--music",
        "song.wav",
        "--length",
        "8s",
        "--durations",
        DURATIONS,
        "--dry-run",
        "--json",
    ]));
    let paths: Vec<&str> = v["project"]["media"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["path"].as_str().unwrap())
        .collect();
    assert_eq!(paths, ["clips/a.mp4", "clips/b.mp4", "clips/c.mp4"]);
}

#[test]
fn errors_are_readable_with_exit_code_1() {
    let dir = workspace();
    let d = dir.path();
    let cases: Vec<(Command, &str)> = vec![
        (
            {
                let mut c = kwikvid(d);
                c.args(["analyze-music", "missing.wav"]);
                c
            },
            "error: cannot analyse music \"missing.wav\"",
        ),
        (
            edit(d, &["--pin", "z.mp4@1-2"]),
            "error: --pin z.mp4@1-2: no clip named \"z.mp4\"",
        ),
        (
            edit(d, &["--pin", "a.mp4@7-9"]),
            "range ends at 9.000 s but \"clips/a.mp4\" is only 8.000 s long",
        ),
        (
            edit(d, &["--pin", "a.mp4@1-3", "--exclude", "a.mp4@2-4"]),
            "error: --exclude a.mp4@2-4:",
        ),
        (
            edit(d, &["--length", "4s", "--pin", "c.mp4@0-6"]),
            "pin clips/c.mp4@0.000-6.000 is longer than the target length 4.000 s",
        ),
        (
            {
                let mut c = kwikvid(d);
                c.args([
                    "edit",
                    "--clips",
                    "clips/nope.mp4",
                    "--music",
                    "song.wav",
                    "--dry-run",
                ]);
                c
            },
            "error: clip \"clips/nope.mp4\" not found",
        ),
        (
            {
                let mut c = kwikvid(d);
                c.args([
                    "edit",
                    "--clips",
                    "clips/a.mp4",
                    "--music",
                    "song.wav",
                    "--dry-run",
                ]);
                c
            },
            "error: cannot probe clip \"clips/a.mp4\"",
        ),
    ];
    for (mut cmd, expected) in cases {
        let err = stderr_of_failure(&mut cmd);
        assert!(err.contains(expected), "expected {expected:?} in {err:?}");
        assert!(!err.contains("panicked"), "{err}");
    }
}

#[test]
fn usage_errors_exit_with_code_2() {
    let dir = workspace();
    for args in [
        &[
            "edit",
            "--clips",
            "clips",
            "--music",
            "song.wav",
            "--length",
            "soon",
            "--dry-run",
        ][..],
        &[
            "edit",
            "--clips",
            "clips",
            "--music",
            "song.wav",
            "--pin",
            "a.mp4",
            "--dry-run",
        ][..],
        &["edit", "--music", "song.wav", "--dry-run"][..],
        // Neither --out nor --dry-run.
        &["edit", "--clips", "clips", "--music", "song.wav"][..],
        &[
            "edit", "--clips", "clips", "--music", "song.wav", "--out", "o.mp4", "--size", "1920",
        ][..],
        &[
            "edit", "--clips", "clips", "--music", "song.wav", "--out", "o.mp4", "--fps", "29.97",
        ][..],
        &["frobnicate"][..],
    ] {
        kwikvid(dir.path()).args(args).assert().failure().code(2);
    }
}

/// A minimal MP4 with GoPro HiLights (`moov/udta/HMMT`, milliseconds), the
/// box `kv_gopro::read_hilights` reads (ADR 0006).
fn hilight_mp4(hilights_ms: &[u32]) -> Vec<u8> {
    fn bx(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut v = ((body.len() + 8) as u32).to_be_bytes().to_vec();
        v.extend_from_slice(kind);
        v.extend_from_slice(body);
        v
    }
    let mut hmmt = (hilights_ms.len() as u32).to_be_bytes().to_vec();
    for ms in hilights_ms {
        hmmt.extend_from_slice(&ms.to_be_bytes());
    }
    let mut file = bx(b"ftyp", b"mp41\0\0\0\0mp41");
    file.extend(bx(b"moov", &bx(b"udta", &bx(b"HMMT", &hmmt))));
    file
}

#[test]
fn dry_run_json_shows_hilights() {
    let dir = workspace();
    // Out of order and one past the clip's 8 s end, which is dropped.
    fs::write(
        dir.path().join("clips/a.mp4"),
        hilight_mp4(&[5_000, 1_500, 9_000]),
    )
    .unwrap();
    let v = json(&mut edit(dir.path(), &["--length", "8s", "--json"]));
    let media = &v["project"]["media"];
    assert_eq!(
        media[0]["hilights"],
        serde_json::json!([1_500_000, 5_000_000])
    );
    // Empty (non-GoPro) files just have none.
    assert_eq!(media[1]["hilights"], serde_json::json!([]));
}

/// Copy `n` tagged clips (2 s each, creation times a minute apart in
/// `GX01000k.MP4` order) into `dir` under `names`.
fn copy_tagged_clips(dir: &Path, names: &[&str]) {
    for (src, name) in tagged_clip_set(names.len(), 2.0).iter().zip(names) {
        fs::copy(src, dir.join(name)).unwrap();
    }
}

#[test]
fn clip_directory_is_in_recording_order_without_proxies() {
    let dir = workspace();
    let shot = dir.path().join("shot");
    fs::create_dir(&shot).unwrap();
    // Recorded first to last: z.mp4, B.MOV, a.MP4; name order would differ.
    copy_tagged_clips(&shot, &["z.mp4", "B.MOV", "a.MP4"]);
    fs::write(shot.join("GL010001.LRV"), b"proxy").unwrap();
    fs::write(shot.join("GX010001.THM"), b"thumb").unwrap();
    let v = json(kwikvid(dir.path()).args([
        "edit",
        "--clips",
        "shot",
        "--music",
        "song.wav",
        "--length",
        "4s",
        "--dry-run",
        "--json",
    ]));
    let paths: Vec<&str> = v["project"]["media"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["path"].as_str().unwrap())
        .collect();
    assert_eq!(paths, ["shot/z.mp4", "shot/B.MOV", "shot/a.MP4"]);
}

/// `edit` over three tagged clips rendering a 4 s edit at `size` to `out`
/// with extra arguments.
fn render_at(dir: &Path, size: &str, out: &str, extra: &[&str]) -> Command {
    copy_tagged_clips(dir, &["c1.mp4", "c2.mp4", "c3.mp4"]);
    let mut cmd = kwikvid(dir);
    cmd.args([
        "edit", "--clips", "c1.mp4", "c2.mp4", "c3.mp4", "--music", "song.wav", "--length", "4s",
        "--size", size, "--out", out,
    ])
    .args(extra);
    cmd
}

/// [`render_at`] 320x180.
fn render(dir: &Path, out: &str, extra: &[&str]) -> Command {
    render_at(dir, "320x180", out, extra)
}

/// One frame at 30 fps, in microseconds.
const FRAME_US: i64 = 1_000_000 / 30;

#[test]
fn edit_renders_an_mp4_with_the_song() {
    let dir = workspace();
    let out = render(dir.path(), "out.mp4", &[])
        .assert()
        .success()
        .get_output()
        .clone();
    assert!(out.stdout.is_empty());
    let stderr = String::from_utf8(out.stderr).unwrap();
    // Not a terminal: periodic lines, then the summary.
    assert!(stderr.contains("rendering   0% (1/120 frames"), "{stderr}");
    assert!(
        stderr.contains("rendering 100% (120/120 frames"),
        "{stderr}"
    );
    // `wrote out.mp4: 3.988 s, encoder libx264, 0.5 s elapsed`: the edit
    // ends on the detected beat nearest 4 s.
    let summary = stderr.lines().last().unwrap();
    let length: f64 = summary
        .strip_prefix("wrote out.mp4: ")
        .and_then(|r| r.split_once(" s, encoder "))
        .map(|(len, _)| len.parse().unwrap())
        .unwrap_or_else(|| panic!("{summary}"));
    assert!((length - 4.0).abs() * 1e6 <= FRAME_US as f64, "{summary}");
    assert!(summary.ends_with(" s elapsed"), "{summary}");

    let info = FfmpegBackend::new()
        .unwrap()
        .probe(&dir.path().join("out.mp4"))
        .unwrap();
    let video = info.video.expect("video stream");
    let audio = info.audio.expect("audio stream");
    assert_eq!(video.codec, "h264");
    assert_eq!((video.width, video.height), (320, 180));
    assert_eq!(audio.codec, "aac");
    assert!(
        (info.duration.0 - 4_000_000).abs() <= FRAME_US,
        "duration {:?}",
        info.duration
    );
    // No temporary file is left behind.
    assert!(!dir.path().join(".out.mp4.partial").exists());
}

#[test]
fn render_refuses_to_replace_without_overwrite() {
    let dir = workspace();
    fs::write(dir.path().join("out.mp4"), b"keep me").unwrap();
    let out = render(dir.path(), "out.mp4", &["--quiet"])
        .assert()
        .failure()
        .code(2)
        .get_output()
        .clone();
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(
        stderr.contains("error: out.mp4 already exists; pass --overwrite"),
        "{stderr}"
    );
    assert_eq!(fs::read(dir.path().join("out.mp4")).unwrap(), b"keep me");

    let out = render(
        dir.path(),
        "out.mp4",
        &["--overwrite", "--quiet", "--fps", "30000/1001"],
    )
    .assert()
    .success()
    .get_output()
    .clone();
    assert!(
        out.stderr.is_empty(),
        "--quiet prints nothing, got: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let info = FfmpegBackend::new()
        .unwrap()
        .probe(&dir.path().join("out.mp4"))
        .unwrap();
    assert_eq!(
        info.video.unwrap().fps,
        kv_media::Rational::new(30000, 1001)
    );
}

#[test]
fn failed_render_leaves_no_file() {
    let dir = workspace();
    // Odd sizes cannot be encoded: rejected before any work.
    let err = String::from_utf8(
        render_at(dir.path(), "321x180", "odd.mp4", &[])
            .assert()
            .failure()
            .code(1)
            .get_output()
            .stderr
            .clone(),
    )
    .unwrap();
    assert!(
        err.contains("error: invalid --size or --fps for export"),
        "{err}"
    );
    assert!(!dir.path().join("odd.mp4").exists());

    // The encoder cannot create the file: fails after the edit is built.
    let err = String::from_utf8(
        render(dir.path(), "missing/out.mp4", &[])
            .assert()
            .failure()
            .code(1)
            .get_output()
            .stderr
            .clone(),
    )
    .unwrap();
    assert!(
        err.starts_with("error: cannot render missing/out.mp4"),
        "{err}"
    );
    assert!(!err.contains("panicked"), "{err}");
    assert!(!dir.path().join("missing").exists());
}
