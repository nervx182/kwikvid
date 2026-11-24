//! End-to-end tests of the `kwikvid` binary on kv-testkit fixtures.
//!
//! Each test runs the binary inside a temporary directory holding a click
//! track (`song.wav`) and empty clip files (`clips/*.mp4`); clip durations come
//! from `--durations`, so no real video is needed. Paths are relative to that
//! directory, which keeps the JSON snapshots machine independent.

use std::fs;
use std::path::Path;

use assert_cmd::cargo::cargo_bin_cmd;
use assert_cmd::Command;
use kv_testkit::{click_track, DEFAULT_SAMPLE_RATE};
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
        (
            {
                let mut c = kwikvid(d);
                c.args([
                    "edit",
                    "--clips",
                    "clips",
                    "--music",
                    "song.wav",
                    "--durations",
                    DURATIONS,
                ]);
                c
            },
            "pass --dry-run",
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
        &["frobnicate"][..],
    ] {
        kwikvid(dir.path()).args(args).assert().failure().code(2);
    }
}
