//! Spawns `kwikvid-mcp`, performs the MCP initialize handshake over stdio,
//! lists tools and resources, and calls every tool.

use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use serde_json::{json, Value};

struct Client {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
}

impl Client {
    /// Spawn the server with the analysis cache in `cache`.
    fn spawn(cache: &Path) -> Self {
        Self::spawn_in(&std::env::temp_dir(), cache)
    }

    /// Spawn the server with media root `root` and the cache in `cache`.
    fn spawn_in(root: &Path, cache: &Path) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_kwikvid-mcp"))
            .arg("--root")
            .arg(root)
            .arg("--cache")
            .arg(cache)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("spawn kwikvid-mcp");
        let stdin = child.stdin.take().unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        Client {
            child,
            stdin,
            stdout,
            next_id: 1,
        }
    }

    fn send(&mut self, msg: &Value) {
        writeln!(self.stdin, "{msg}").unwrap();
        self.stdin.flush().unwrap();
    }

    /// Send a request and return its `result`, panicking on a JSON-RPC error.
    fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        loop {
            let mut line = String::new();
            let n = self.stdout.read_line(&mut line).unwrap();
            assert!(n > 0, "server closed stdout while waiting for {method}");
            let msg: Value = serde_json::from_str(&line)
                .unwrap_or_else(|e| panic!("stdout line is not JSON ({e}): {line}"));
            if msg["id"] == json!(id) {
                if let Some(err) = msg.get("error") {
                    panic!("{method} failed: {err}");
                }
                return msg["result"].clone();
            }
            // Anything else (notifications, logs) is skipped.
        }
    }

    /// Call a tool; returns (is_error, text of the first content block).
    fn call(&mut self, name: &str, args: Value) -> (bool, String) {
        let r = self.request("tools/call", json!({"name": name, "arguments": args}));
        let is_error = r["isError"].as_bool().unwrap_or(false);
        let text = r["content"][0]["text"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        (is_error, text)
    }

    fn call_ok(&mut self, name: &str, args: Value) -> Value {
        let (is_error, text) = self.call(name, args);
        assert!(!is_error, "{name} returned an error: {text}");
        serde_json::from_str(&text).unwrap_or_else(|e| panic!("{name} text is not JSON ({e})"))
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Client {
    fn initialize(&mut self) -> Value {
        let init = self.request(
            "initialize",
            json!({
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {"name": "kv-mcp-test", "version": "0"}
            }),
        );
        self.send(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
        init
    }
}

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
            "edit_exclude_range",
            "edit_pin_range",
            "edit_redo",
            "edit_regenerate",
            "edit_reroll",
            "edit_set_target_length",
            "edit_undo",
            "edit_unexclude_range",
            "edit_unpin_range",
            "media_add",
            "media_import",
            "media_remove",
            "music_beats",
            "music_clear",
            "music_set",
            "project_state"
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
