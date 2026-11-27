//! Spawns `kwikvid-mcp`, performs the MCP initialize handshake over stdio,
//! lists tools and resources, and calls every tool.

mod common;

use common::Client;
use serde_json::{json, Value};

#[test]
fn server_starts_when_the_cache_dir_cannot_be_created() {
    // A directory below a regular file cannot be created: the cache falls
    // back to memory and the server still serves.
    let file = std::env::temp_dir().join(format!("kv-mcp-not-a-dir-{}", std::process::id()));
    std::fs::write(&file, b"").unwrap();
    let mut c = Client::spawn(&file.join("cache"));
    c.initialize();
    let state = c.call_ok("project_state", json!({}));
    assert_eq!(state["revision"], 0);
    drop(c);
    let _ = std::fs::remove_file(&file);
}

#[test]
fn handshake_list_and_call_every_tool() {
    let cache = std::env::temp_dir().join(format!("kv-mcp-cache-{}", std::process::id()));
    let mut c = Client::spawn(&cache);

    // Handshake.
    let init = c.initialize();
    assert_eq!(init["serverInfo"]["name"], "kwikvid-mcp");
    assert!(init["capabilities"]["tools"].is_object());
    assert!(init["capabilities"]["resources"].is_object());
    let instructions = init["instructions"].as_str().unwrap();

    // Tools, with schemas generated from the Rust parameter types.
    let tools = c.request("tools/list", json!({}));
    let tools = tools["tools"].as_array().unwrap();
    let mut names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    names.sort_unstable();
    assert_eq!(
        names,
        [
            "aspect_set",
            "clip_audio_set",
            "edit_exclude_range",
            "edit_move_cut",
            "edit_move_segment",
            "edit_nudge_cut",
            "edit_pin_range",
            "edit_redo",
            "edit_regenerate",
            "edit_reroll",
            "edit_set_target_length",
            "edit_undo",
            "edit_unexclude_range",
            "edit_unpin_range",
            "export_cancel",
            "export_options",
            "export_start",
            "export_status",
            "media_add",
            "media_import",
            "media_remove",
            "music_beats",
            "music_clear",
            "music_set",
            "music_volume_set",
            "project_new",
            "project_open",
            "project_save",
            "project_save_as",
            "project_state",
            "style_set",
            "title_clear",
            "title_set"
        ]
    );
    for name in &names {
        assert!(instructions.contains(name), "{name} not in instructions");
    }
    let schema =
        |name: &str| tools.iter().find(|t| t["name"] == name).unwrap()["inputSchema"].clone();
    let media_add = schema("media_add");
    assert_eq!(media_add["required"], json!(["path"]));
    assert!(media_add["properties"]["duration_us"].is_object());
    assert_eq!(schema("media_remove")["required"], json!(["media_id"]));
    assert_eq!(schema("media_import")["required"], json!(["paths"]));
    assert_eq!(schema("music_set")["required"], json!(["path"]));
    let length = schema("edit_set_target_length");
    for field in ["seconds", "full_song"] {
        let doc = length["properties"][field]["description"].as_str().unwrap();
        assert!(doc.contains("either"), "{field}: {doc}");
    }
    for tool in [
        "edit_pin_range",
        "edit_unpin_range",
        "edit_exclude_range",
        "edit_unexclude_range",
    ] {
        let s = schema(tool);
        for field in ["media_id", "start_us", "end_us"] {
            assert!(
                s["required"].as_array().unwrap().contains(&json!(field)),
                "{tool}: {field} should be required: {s}"
            );
        }
    }

    // The look tools take their values from JSON-schema enums.
    let enum_values = |schema: &Value, field: &str| -> Vec<String> {
        // The enum may be inline or behind a $ref into $defs.
        let prop = &schema["properties"][field];
        let target = match prop["$ref"].as_str() {
            Some(r) => &schema["$defs"][r.rsplit('/').next().unwrap()],
            None => prop,
        };
        let mut values: Vec<String> = match target["enum"].as_array() {
            Some(v) => v.iter().map(|v| v.as_str().unwrap().to_owned()).collect(),
            None => target["oneOf"]
                .as_array()
                .unwrap_or_else(|| panic!("{field}: no enum in {schema}"))
                .iter()
                .map(|v| {
                    v["const"]
                        .as_str()
                        .or_else(|| v["enum"][0].as_str())
                        .unwrap()
                        .to_owned()
                })
                .collect(),
        };
        values.sort_unstable();
        values
    };
    let style_set = schema("style_set");
    assert_eq!(style_set["required"], json!(["style"]));
    assert_eq!(
        enum_values(&style_set, "style"),
        ["action", "cut", "smooth"]
    );
    let aspect_set = schema("aspect_set");
    assert_eq!(aspect_set["required"], json!(["aspect"]));
    assert_eq!(
        enum_values(&aspect_set, "aspect"),
        ["landscape16x9", "portrait9x16", "square1x1"]
    );
    assert_eq!(
        schema("music_volume_set")["properties"]["percent"]["maximum"],
        100
    );
    assert_eq!(schema("title_set")["required"], json!(["text"]));
    assert_eq!(
        schema("edit_nudge_cut")["properties"]["beats"]["type"],
        "integer"
    );

    // project_state on an empty project: the session snapshot.
    let state = c.call_ok("project_state", json!({}));
    assert_eq!(state["revision"], 0);
    assert_eq!(state["can_undo"], false);
    assert_eq!(state["can_redo"], false);
    assert_eq!(state["dirty"], false);
    assert!(state["media_root"].is_string());
    assert_eq!(state["project"]["version"], 1);
    assert_eq!(state["project"]["media"], json!([]));

    // media_add with a duration override (no file access).
    let r = c.call_ok(
        "media_add",
        json!({"path": "clips/GX010001.MP4", "duration_us": 10_000_000}),
    );
    assert_eq!(
        r,
        json!({
            "revision": 1,
            "can_undo": true,
            "can_redo": false,
            "dirty": true,
            "manual_edits_replaced": false,
            "regen_error": r["regen_error"],
            "edit": {"segments": 0, "length_us": 0},
            "media_id": 1
        })
    );
    // No music: the edit cannot be built, but the media is added.
    assert!(r["regen_error"].is_string(), "{r}");
    let r = c.call_ok(
        "media_add",
        json!({"path": "b.mp4", "duration_us": 5_000_000}),
    );
    assert_eq!(r["media_id"], 2);
    let r = c.call_ok(
        "media_add",
        json!({"path": "c.mp4", "duration_us": 5_000_000}),
    );
    assert_eq!(r["media_id"], 3);

    // Pin and exclude.
    let r = c.call_ok(
        "edit_pin_range",
        json!({"media_id": 1, "start_us": 1_000_000, "end_us": 3_000_000}),
    );
    assert_eq!(r["revision"], 4);
    assert!(r.get("media_id").is_none(), "{r}");
    c.call_ok(
        "edit_exclude_range",
        json!({"media_id": 2, "start_us": 0, "end_us": 5_000_000}),
    );

    // A rejected command is a tool error carrying a readable message, and
    // leaves the project unchanged.
    let before = c.call_ok("project_state", json!({}));
    let (is_error, text) = c.call(
        "edit_exclude_range",
        json!({"media_id": 1, "start_us": 2_000_000, "end_us": 4_000_000}),
    );
    assert!(is_error);
    assert!(text.contains("overlaps"), "{text}");
    let (is_error, _) = c.call("media_add", json!({"path": "../x.mp4", "duration_us": 1}));
    assert!(is_error);

    let state = c.call_ok("project_state", json!({}));
    assert_eq!(state, before);
    let project = &state["project"];
    assert_eq!(project["media"][0]["path"], "clips/GX010001.MP4");
    assert_eq!(project["media"][0]["duration"], 10_000_000);
    let section = &project["sections"][0];
    assert_eq!(
        section["pins"],
        json!([{"media_id": 1, "start": 1_000_000, "end": 3_000_000}])
    );
    assert_eq!(
        section["excludes"],
        json!([{"media_id": 2, "start": 0, "end": 5_000_000}])
    );

    // Resource returns the same JSON as project_state.
    let resources = c.request("resources/list", json!({}));
    assert_eq!(resources["resources"][0]["uri"], "project://current");
    let read = c.request("resources/read", json!({"uri": "project://current"}));
    let text = read["contents"][0]["text"].as_str().unwrap();
    assert_eq!(serde_json::from_str::<Value>(text).unwrap(), state);

    // Unexclude and unpin, then remove a clip.
    c.call_ok(
        "edit_unexclude_range",
        json!({"media_id": 2, "start_us": 0, "end_us": 5_000_000}),
    );
    c.call_ok(
        "edit_unpin_range",
        json!({"media_id": 1, "start_us": 1_000_000, "end_us": 3_000_000}),
    );
    c.call_ok("media_remove", json!({"media_id": 3}));
    let state = c.call_ok("project_state", json!({}));
    assert_eq!(state["project"]["sections"][0]["pins"], json!([]));
    assert_eq!(state["project"]["sections"][0]["excludes"], json!([]));
    assert_eq!(state["project"]["media"].as_array().unwrap().len(), 2);

    // Undo the remove and redo it.
    let r = c.call_ok("edit_undo", json!({}));
    assert_eq!(r["can_redo"], true);
    let state = c.call_ok("project_state", json!({}));
    assert_eq!(state["project"]["media"].as_array().unwrap().len(), 3);
    let r = c.call_ok("edit_redo", json!({}));
    assert_eq!(r["can_redo"], false);
    let (is_error, text) = c.call("edit_redo", json!({}));
    assert!(is_error);
    assert_eq!(text, "Nothing to redo.");

    // Undo everything (3 adds, pin, exclude, unexclude, unpin, remove); then
    // there is nothing left to undo.
    for _ in 0..8 {
        c.call_ok("edit_undo", json!({}));
    }
    let state = c.call_ok("project_state", json!({}));
    assert_eq!(state["project"]["media"], json!([]));
    assert_eq!(state["dirty"], false);
    let (is_error, text) = c.call("edit_undo", json!({}));
    assert!(is_error);
    assert_eq!(text, "Nothing to undo.");

    // Look and tweak tools on the empty project.
    c.call_ok("aspect_set", json!({"aspect": "portrait9x16"}));
    c.call_ok("style_set", json!({"style": "smooth"}));
    c.call_ok("music_volume_set", json!({"percent": 60}));
    c.call_ok("title_set", json!({"text": "Day one"}));
    let state = c.call_ok("project_state", json!({}));
    let project = &state["project"];
    assert_eq!(project["settings"]["aspect_ratio"], "portrait9x16");
    assert_eq!(project["settings"]["music_volume_percent"], 60);
    assert_eq!(project["sections"][0]["style"], "smooth");
    assert_eq!(
        project["sections"][0]["title"],
        json!({"text": "Day one", "duration": 3_000_000})
    );
    c.call_ok("title_clear", json!({}));
    let (is_error, text) = c.call("title_set", json!({"text": "x".repeat(81)}));
    assert!(is_error && text.contains("title"), "{text}");
    let (is_error, text) = c.call("music_volume_set", json!({"percent": 101}));
    assert!(is_error && text.contains("between 0 and 100"), "{text}");
    let (is_error, _) = c.call("clip_audio_set", json!({"media_id": 9, "enabled": false}));
    assert!(is_error);
    // No edit yet: segment and cut indices are out of range.
    for (tool, args) in [
        ("edit_move_segment", json!({"from": 0, "to": 1})),
        ("edit_nudge_cut", json!({"cut": 1, "beats": 1})),
        ("edit_move_cut", json!({"cut": 1, "to_us": 1_000_000})),
    ] {
        let (is_error, text) = c.call(tool, args);
        assert!(
            is_error && text.contains("no longer in the timeline"),
            "{tool}: {text}"
        );
    }
    drop(c);
    assert!(cache.join("cache.sqlite").exists());
    let _ = std::fs::remove_dir_all(&cache);
}

/// FFmpeg-backed: 3 tagged clips and a click track in a temp `--root`;
/// `media_import` the folder, `music_set`, `edit_set_target_length` 10 s,
/// and the edit is built.
#[test]
fn import_music_and_target_length_build_an_edit() {
    let root = tempfile::tempdir().unwrap();
    let clips = root.path().join("clips");
    std::fs::create_dir(&clips).unwrap();
    for clip in kv_testkit::tagged_clip_set(3, 6.0) {
        std::fs::copy(&clip, clips.join(clip.file_name().unwrap())).unwrap();
    }
    let song = kv_testkit::click_track(120.0, 20.0, kv_testkit::DEFAULT_SAMPLE_RATE);
    std::fs::copy(&song, root.path().join("song.wav")).unwrap();
    let cache = root.path().join(".cache");

    let mut c = Client::spawn_in(root.path(), &cache);
    c.initialize();
    let r = c.call_ok("media_import", json!({"paths": ["clips"]}));
    assert_eq!(r["added"], json!([1, 2, 3]), "{r}");
    assert_eq!(r["skipped"], json!([]), "{r}");
    assert_eq!(r["revision"], 1);

    let r = c.call_ok("music_set", json!({"path": "song.wav"}));
    let bpm = r["tempo_bpm"].as_f64().unwrap();
    assert!((bpm - 120.0).abs() < 2.0, "{r}");
    assert!(r["beats"].as_u64().unwrap() >= 35, "{r}");
    // The default target is the full 20 s song: more than 18 s of footage.
    assert!(r["regen_error"].is_string(), "{r}");

    let r = c.call_ok("edit_set_target_length", json!({"seconds": 10}));
    assert_eq!(r["regen_error"], Value::Null, "{r}");
    assert!(r["edit"]["segments"].as_u64().unwrap() > 0, "{r}");
    let len = r["edit"]["length_us"].as_i64().unwrap();
    assert!((len - 10_000_000).abs() <= 500_000, "{r}");

    let beats = c.call_ok("music_beats", json!({}));
    assert!(beats["beats_us"].as_array().unwrap().len() >= 35, "{beats}");
    let r = c.call_ok("edit_reroll", json!({}));
    assert!(r["edit"]["segments"].as_u64().unwrap() > 0, "{r}");

    let (is_error, text) = c.call("music_set", json!({"path": "clips/missing.wav"}));
    assert!(is_error, "{text}");
}

/// A temp `--root` with 3 tagged clips in `clips/` and a 20 s click track
/// `song.wav`, and a server on it with an edit of `seconds` built.
fn edit_in_temp_root(seconds: u64) -> (tempfile::TempDir, Client) {
    let root = tempfile::tempdir().unwrap();
    let clips = root.path().join("clips");
    std::fs::create_dir(&clips).unwrap();
    for clip in kv_testkit::tagged_clip_set(3, 6.0) {
        std::fs::copy(&clip, clips.join(clip.file_name().unwrap())).unwrap();
    }
    let song = kv_testkit::click_track(120.0, 20.0, kv_testkit::DEFAULT_SAMPLE_RATE);
    std::fs::copy(&song, root.path().join("song.wav")).unwrap();
    let mut c = Client::spawn_in(root.path(), &root.path().join(".cache"));
    c.initialize();
    c.call_ok("media_import", json!({"paths": ["clips"]}));
    c.call_ok("music_set", json!({"path": "song.wav"}));
    let r = c.call_ok("edit_set_target_length", json!({"seconds": seconds}));
    assert_eq!(r["regen_error"], Value::Null, "{r}");
    (root, c)
}

/// FFmpeg-backed: export a 3 s edit at 1080p, wait for `done`, and probe
/// the file; then save the project, start a new one, open it again and
/// regenerate.
#[test]
fn export_and_project_round_trip() {
    use kv_media::MediaBackend;
    let (root, mut c) = edit_in_temp_root(3);

    let options = c.call_ok("export_options", json!({}));
    assert_eq!(options["blocked"], Value::Null, "{options}");
    assert_eq!(options["codecs"][0], "h264", "{options}");
    assert_eq!(options["file_name"], "Untitled.mp4");

    let started = c.call_ok("export_start", json!({"path": "out.mp4"}));
    assert_eq!(started["width"], 1920, "{started}");
    assert_eq!(started["height"], 1080, "{started}");
    assert_eq!(started["fps"], 30, "{started}");
    let id = started["id"].as_u64().unwrap();
    // A second export while the first runs is refused (unless it already
    // finished, which a 90-frame 1080p export does not do this fast).
    let (is_error, text) = c.call("export_start", json!({"path": "two.mp4"}));
    assert!(is_error && text.contains("still running"), "{text}");

    let mut status = Value::Null;
    for _ in 0..20 {
        status = c.call_ok("export_status", json!({"id": id, "wait_ms": 30_000}));
        if status["state"] != "started" && status["state"] != "progress" {
            break;
        }
    }
    assert_eq!(status["state"], "done", "{status}");
    assert_eq!(status["id"], id);
    assert_eq!(status["report"]["path"], "out.mp4", "{status}");
    let out = root.path().join("out.mp4");
    let info = kv_media::FfmpegBackend::new().unwrap().probe(&out).unwrap();
    let d = info.duration.0;
    assert!((d - 3_000_000).abs() <= 200_000, "duration {d}");
    let video = info.video.unwrap();
    assert_eq!((video.width, video.height), (1920, 1080));
    let (is_error, _) = c.call("export_cancel", json!({"id": id}));
    assert!(is_error, "cancel after done is an error");

    // Save, new, open: the same project, music analysed again.
    let saved = c.call_ok("project_save_as", json!({"path": "trip"}));
    assert_eq!(saved["file"], "trip.kwikvid");
    let before = c.call_ok("project_state", json!({}))["project"].clone();
    let r = c.call_ok("project_new", json!({}));
    assert_eq!(r["file"], Value::Null);
    assert_eq!(r["edit"]["segments"], 0);
    let r = c.call_ok("project_open", json!({"path": "trip.kwikvid"}));
    assert_eq!(r["missing"], json!([]), "{r}");
    assert!(r.get("analysis_errors").is_none(), "{r}");
    assert_eq!(c.call_ok("project_state", json!({}))["project"], before);
    let r = c.call_ok("edit_regenerate", json!({}));
    assert!(r["edit"]["segments"].as_u64().unwrap() > 0, "{r}");
    assert_eq!(c.call_ok("project_save", json!({}))["dirty"], false);
}

/// FFmpeg-backed: cancel a running export; it reaches `cancelled` and
/// leaves no file (nor its partial file).
#[test]
fn cancelled_export_leaves_no_file() {
    let (root, mut c) = edit_in_temp_root(10);
    let started = c.call_ok("export_start", json!({"path": "cut.mp4"}));
    let id = started["id"].as_u64().unwrap();
    c.call_ok("export_cancel", json!({"id": id}));
    let status = c.call_ok("export_status", json!({"id": id, "wait_ms": 30_000}));
    assert_eq!(status["state"], "cancelled", "{status}");
    assert!(!root.path().join("cut.mp4").exists());
    assert!(!root.path().join(".cut.mp4.partial").exists());
}
