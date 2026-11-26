# kv-mcp

The kwikvid MCP server. The `kwikvid-mcp` binary speaks the
[Model Context Protocol](https://modelcontextprotocol.io) over stdio and holds
one `kv_session::Session`: the same project, undo history and auto-edit
rebuild the desktop app uses. Every write tool maps onto a `kv_core::Command`
applied with `Session::apply_and_regenerate`, so pins, excludes and media
changes rebuild the edit exactly as in the app (ADR 0010), and one undo
reverts the command together with the rebuild it caused.

| Tool | kv-core / kv-session | Input |
| --- | --- | --- |
| `project_state` | `Session::snapshot` (read) | none |
| `media_add` | `AddMedia` | `path`, optional `duration_us` (skips probing), `kind`, `hilights_us` |
| `media_remove` | `RemoveMedia` (drops its pins, excludes and cuts) | `media_id` |
| `edit_pin_range` | `PinRange` | `media_id`, `start_us`, `end_us`, optional `section` |
| `edit_unpin_range` | `UnpinRange` | same as `edit_pin_range` (must equal a pin) |
| `edit_exclude_range` | `ExcludeRange` | same as `edit_pin_range` |
| `edit_unexclude_range` | `UnexcludeRange` | same as `edit_pin_range` (must equal an exclude) |
| `edit_undo` | `Session::undo` | none |
| `edit_redo` | `Session::redo` | none |

## Results

`project_state` and the resource `project://current` return the
`kv_session::Snapshot` JSON:

```json
{"revision": 3, "project": {"version": 1, "media": [...], "sections": [...], ...},
 "can_undo": true, "can_redo": false, "dirty": true, "media_root": "/footage"}
```

Every write tool (including `edit_undo` / `edit_redo`) returns:

```json
{"revision": 4, "can_undo": true, "can_redo": false, "dirty": true,
 "manual_edits_replaced": false, "regen_error": null,
 "edit": {"segments": 12, "length_us": 30000000}, "media_id": 1}
```

- `revision`: bumped by every change.
- `manual_edits_replaced`: the rebuild replaced manual tweaks of the edit
  (`edit_undo` brings them back).
- `regen_error`: why the edit could not be rebuilt (no clips, no music, music
  analysis pending, pins too long, ...); the command itself was applied.
  `null` when the edit was rebuilt or no rebuild was due. Undo and redo never
  rebuild, so for them it is always `null`.
- `edit`: the edit after the call, summed over sections: segment count and
  total timeline length in microseconds.
- `media_id`: only from `media_add`, the new item's id.

A rejected command (unknown media, overlapping ranges, nothing to undo, a path
outside the root, ...) is a tool result with `isError: true` and a short
message; the project is unchanged.

All times are integer microseconds. Media paths are relative to `--root <dir>`
(default: the working directory), or absolute inside it; the server refuses
paths outside it. Without `duration_us`, `media_add` probes the file with
FFmpeg (see `kv-media`); when FFmpeg cannot be loaded the server still starts
and probing returns an error saying so. Music analyses are cached in
`cache.sqlite` in `--cache <dir>` (default: `kwikvid/` in the platform cache
directory, in memory when it cannot be opened). Logs go to stderr; set
`RUST_LOG=kv_mcp=debug` for more.

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
