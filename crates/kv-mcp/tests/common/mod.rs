//! The stdio JSON-RPC client shared by the integration tests: spawns
//! `kwikvid-mcp` and calls its tools the way an MCP client would.

#![allow(dead_code)] // Each test crate uses a different part of it.

use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use serde_json::{json, Value};

pub struct Client {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
}

impl Client {
    /// Spawn the server with the analysis cache in `cache`.
    pub fn spawn(cache: &Path) -> Self {
        Self::spawn_in(&std::env::temp_dir(), cache)
    }

    /// Spawn the server with media root `root` and the cache in `cache`.
    pub fn spawn_in(root: &Path, cache: &Path) -> Self {
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

    pub fn send(&mut self, msg: &Value) {
        writeln!(self.stdin, "{msg}").unwrap();
        self.stdin.flush().unwrap();
    }

    /// Send a request and return its `result`, panicking on a JSON-RPC error.
    pub fn request(&mut self, method: &str, params: Value) -> Value {
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
    pub fn call(&mut self, name: &str, args: Value) -> (bool, String) {
        let r = self.request("tools/call", json!({"name": name, "arguments": args}));
        let is_error = r["isError"].as_bool().unwrap_or(false);
        let text = r["content"][0]["text"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        (is_error, text)
    }

    pub fn call_ok(&mut self, name: &str, args: Value) -> Value {
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
    pub fn initialize(&mut self) -> Value {
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
