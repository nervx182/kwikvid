# 0001 Rust workspace, headless core first

Status: accepted (2026-11-24)

Context: kwikvid must be Rust, macOS Apple Silicon first, cross-platform in mind, developed only in Linux cloud sessions.

Decision: one Cargo workspace; pure core crates; desktop app, CLI and MCP server are thin front ends over one Command API in kv-core.

Consequences: nearly everything is testable headless on Linux; platform code is isolated behind traits.
