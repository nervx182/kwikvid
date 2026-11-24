# kv-mcp

The kwikvid MCP server. The `kwikvid-mcp` binary speaks the
[Model Context Protocol](https://modelcontextprotocol.io) over stdio and holds
one in-memory project. Every write tool maps onto a `kv_core::Command`, the
same API the desktop app and CLI use.

| Tool | kv-core | Input |
| --- | --- | --- |
| `project_state` | (read) | none |
| `media_add` | `AddMedia` | `path`, optional `duration_us` (skips probing), `kind`, `hilights_us` |
| `edit_pin_range` | `PinRange` | `media_id`, `start_us`, `end_us`, optional `section` |
| `edit_exclude_range` | `ExcludeRange` | same as `edit_pin_range` |
| `edit_undo` | `History::undo` | none |

Resource `project://current` returns the same JSON as `project_state`. All
times are integer microseconds. Media paths are relative to `--root <dir>`
(default: the working directory), or absolute inside it. Without
`duration_us`, `media_add` probes the file with FFmpeg (see `kv-media`).
Logs go to stderr; set `RUST_LOG=kv_mcp=debug` for more.

## Build

```sh
cargo build --release -p kv-mcp   # target/release/kwikvid-mcp
```

## Register with Claude Code

```sh
claude mcp add kwikvid -- /path/to/kwikvid/target/release/kwikvid-mcp --root /path/to/footage
```

## Register with Claude Desktop

Add to `claude_desktop_config.json` (macOS:
`~/Library/Application Support/Claude/claude_desktop_config.json`), then
restart Claude Desktop:

```json
{
  "mcpServers": {
    "kwikvid": {
      "command": "/path/to/kwikvid/target/release/kwikvid-mcp",
      "args": ["--root", "/path/to/footage"]
    }
  }
}
```
