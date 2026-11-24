//! Spawns `kwikvid-mcp`, performs the MCP initialize handshake over stdio,
//! lists tools and resources, and calls every tool.

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use serde_json::{json, Value};

struct Client {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
}

impl Client {
    fn spawn() -> Self {
        let root = std::env::temp_dir();
        let mut child = Command::new(env!("CARGO_BIN_EXE_kwikvid-mcp"))
            .arg("--root")
            .arg(&root)
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

#[test]
fn handshake_list_and_call_every_tool() {
    let mut c = Client::spawn();

    // Handshake.
    let init = c.request(
        "initialize",
        json!({
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": {"name": "kv-mcp-test", "version": "0"}
        }),
    );
    assert_eq!(init["serverInfo"]["name"], "kwikvid-mcp");
    assert!(init["capabilities"]["tools"].is_object());
    assert!(init["capabilities"]["resources"].is_object());
    c.send(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));

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
            "edit_undo",
            "media_add",
            "project_state"
        ]
    );
    let schema =
        |name: &str| tools.iter().find(|t| t["name"] == name).unwrap()["inputSchema"].clone();
    let media_add = schema("media_add");
    assert_eq!(media_add["required"], json!(["path"]));
    assert!(media_add["properties"]["duration_us"].is_object());
    let pin = schema("edit_pin_range");
    for field in ["media_id", "start_us", "end_us"] {
        assert!(
            pin["required"].as_array().unwrap().contains(&json!(field)),
            "{field} should be required: {pin}"
        );
    }

    // project_state on an empty project.
    let state = c.call_ok("project_state", json!({}));
    assert_eq!(state["version"], 1);
    assert_eq!(state["media"], json!([]));

    // media_add with a duration override (no file access).
    let ev = c.call_ok(
        "media_add",
        json!({"path": "clips/GX010001.MP4", "duration_us": 10_000_000}),
    );
    assert_eq!(ev, json!({"type": "media_added", "media_id": 1}));
    let ev = c.call_ok(
        "media_add",
        json!({"path": "b.mp4", "duration_us": 5_000_000}),
    );
    assert_eq!(ev["media_id"], 2);

    // Pin and exclude.
    let ev = c.call_ok(
        "edit_pin_range",
        json!({"media_id": 1, "start_us": 1_000_000, "end_us": 3_000_000}),
    );
    assert_eq!(ev["type"], "range_pinned");
    assert_eq!(ev["section"], 1);
    let ev = c.call_ok(
        "edit_exclude_range",
        json!({"media_id": 2, "start_us": 0, "end_us": 5_000_000}),
    );
    assert_eq!(ev["type"], "range_excluded");

    // A rejected command is a tool error carrying the kv-core message, and
    // leaves the project unchanged.
    let (is_error, text) = c.call(
        "edit_exclude_range",
        json!({"media_id": 1, "start_us": 2_000_000, "end_us": 4_000_000}),
    );
    assert!(is_error);
    assert!(text.contains("overlaps"), "{text}");

    let state = c.call_ok("project_state", json!({}));
    assert_eq!(state["media"][0]["path"], "clips/GX010001.MP4");
    assert_eq!(state["media"][0]["duration"], 10_000_000);
    let section = &state["sections"][0];
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

    // Undo the exclude, then the pin.
    let ev = c.call_ok("edit_undo", json!({}));
    assert_eq!(ev["undone"]["type"], "exclude_range");
    let ev = c.call_ok("edit_undo", json!({}));
    assert_eq!(ev["undone"]["type"], "pin_range");
    let state = c.call_ok("project_state", json!({}));
    assert_eq!(state["sections"][0]["pins"], json!([]));
    assert_eq!(state["sections"][0]["excludes"], json!([]));
    assert_eq!(state["media"].as_array().unwrap().len(), 2);

    // Undo both adds; then there is nothing left to undo.
    c.call_ok("edit_undo", json!({}));
    c.call_ok("edit_undo", json!({}));
    let (is_error, text) = c.call("edit_undo", json!({}));
    assert!(is_error);
    assert_eq!(text, "nothing to undo");
}
