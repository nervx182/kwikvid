# kv-mcp

The kwikvid MCP server. The `kwikvid-mcp` binary speaks the
[Model Context Protocol](https://modelcontextprotocol.io) over stdio and holds
one `kv_session::Session`: the same project, undo history and auto-edit
rebuild the desktop app uses. Every write tool maps onto a `kv_core::Command`
applied with `Session::apply_and_regenerate`, so pins, excludes, media
changes and style choices rebuild the edit exactly as in the app (ADR 0010),
while manual tweaks (reorder, cut moves, audio, volume) never do, and one undo
reverts the command together with the rebuild it caused.

| Tool | kv-core / kv-session | Input |
| --- | --- | --- |
| `project_state` | `Session::snapshot` (read) | none |
| `media_add` | `AddMedia` | `path`, optional `duration_us` (skips probing), `kind`, `hilights_us` |
| `media_import` | `Session::import` (one undo step; GoPro chapters grouped; no rebuild) | `paths` (files and folders) |
| `media_remove` | `RemoveMedia` (drops its pins, excludes and cuts) | `media_id` |
| `edit_pin_range` | `PinRange` | `media_id`, `start_us`, `end_us`, optional `section` |
| `edit_unpin_range` | `UnpinRange` | same as `edit_pin_range` (must equal a pin) |
| `edit_exclude_range` | `ExcludeRange` | same as `edit_pin_range` |
| `edit_unexclude_range` | `UnexcludeRange` | same as `edit_pin_range` (must equal an exclude) |
| `edit_undo` | `Session::undo` | none |
| `edit_redo` | `Session::redo` | none |
| `music_set` | `Job::AnalyzeMusic` (cached), then `SetMusic` | `path`, optional `section` |
| `music_clear` | `SetMusic` with no music | optional `section` |
| `music_beats` | the section's music analysis (read) | optional `section` |
| `edit_set_target_length` | `SetTargetLength` (`Fixed` or `FullSong`) | exactly one of `seconds` or `full_song: true`, optional `section` |
| `edit_regenerate` | `Session::regenerate` | optional `section` |
| `edit_reroll` | `Session::reroll` (next seed) | optional `section` |
| `edit_move_segment` | `MoveSegment` (tweak, no rebuild) | `from`, `to` (segment indices from 0), optional `section` |
| `edit_nudge_cut` | `Session::nudge_cut` → `MoveCut` (tweak, no rebuild) | `cut` (1..segments), `beats` (negative: earlier; 0 snaps to the nearest beat), optional `section` |
| `edit_move_cut` | `MoveCut` (tweak, no rebuild) | `cut`, `to_us` (timeline time, off the beat grid if need be), optional `section` |
| `clip_audio_set` | `SetClipAudio` (tweak, no rebuild) | `media_id`, `enabled` |
| `music_volume_set` | `SetMusicVolume` (tweak, no rebuild) | `percent` (0-100) |
| `style_set` | `SetStyle` (rebuilds the edit) | `style`: `cut`, `smooth` or `action`; optional `section` |
| `aspect_set` | `SetAspectRatio` (no rebuild) | `aspect`: `landscape16x9` (16:9 landscape), `portrait9x16` (9:16 portrait) or `square1x1` (1:1 square) |
| `title_set` | `SetTitleCard` (no rebuild) | `text` (1-80 characters), optional `duration_us` (0.5-5 s, default 3 s), optional `section` |
| `title_clear` | `SetTitleCard` with no title (no rebuild) | optional `section` |

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

Some tools add keys to the write result:

- `media_import`: `added` (new media ids, one per recording) and `skipped`
  (`[{"path": ..., "reason": ...}]`). Import does not rebuild the edit (as in
  the app); call `edit_regenerate` afterwards when music is set.
- `music_set`: `tempo_bpm`, `beats` (count) and `duration_us` of the analysed
  song. The song is analysed before the call returns; a song that cannot be
  analysed is an error and changes nothing.

`music_beats` returns `{"path", "tempo_bpm", "duration_us", "beats_us",
"downbeats_us"}`. `edit_regenerate` and `edit_reroll` return an error with
the user-facing reason ("Add music to create an edit.", "Pinned moments are
longer than the target length.", ...) when the edit cannot be built.

A rejected command (unknown media, overlapping ranges, nothing to undo, a path
outside the root, ...) is a tool result with `isError: true` and a short
message; the project is unchanged.

All times are integer microseconds, except `seconds` of
`edit_set_target_length`. Media paths are relative to `--root <dir>`
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
