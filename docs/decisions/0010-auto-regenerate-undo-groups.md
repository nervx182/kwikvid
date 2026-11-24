# 0010 Auto-regenerate on constraint changes; one undo step per action

Status: accepted (2026-11-24)

## Context

MVP items 5-7 (`docs/roadmap.md`): the auto-edit follows the user's
constraints (music, target length, pins, excludes, style, clips), and re-roll
keeps pinned ranges. `kv_autoedit::generate` is pure; the project stores its
inputs and its result, but something has to connect them. In GoPro Quik a
constraint change rebuilds the edit immediately. A naive implementation
applies the constraint and then `ReplaceEdl` as two undo steps, so one undo
after a pin would bring back the old edit with the pin still set.

## Decision

- `kv-session` rebuilds the edit after `SetMusic`, `SetTargetLength`,
  `PinRange`, `UnpinRange`, `ExcludeRange`, `UnexcludeRange`, `SetStyle`,
  `AddMedia` and `RemoveMedia` (`Session::apply_and_regenerate`). Manual
  tweaks (`MoveSegment`, `MoveCut`) do not rebuild. A rebuild replaces manual
  tweaks; the result carries `manual_edits_replaced` so the UI can offer
  "Manual edits replaced (Undo)".
- `kv_core::History::apply_group` records several commands as **one** undo
  step. The session applies a user command and the `ReplaceEdl`s it causes as
  one group, so undo restores both the constraint and the previous edit. The
  grouping lives in kv-core, not kv-session, because the history's
  snapshot-per-step design makes it a few lines there and lets the CLI and
  MCP server use it too.
- A rebuild that fails after a trigger (no music, analysis pending, pins too
  long) does not block the action: the command applies alone, the previous
  edit stays, and the result carries a short message (`regen_error`).
  Explicit `regenerate` / `reroll` return the error and change nothing.
- Re-roll uses `seed + 1`, skipping ahead (up to 8 seeds) while the edit
  stays identical.

## Consequences

- The CLI and MCP server still apply plain commands with no auto-rebuild;
  they can call the same session methods or `History::apply_group` later.
- Detecting manual tweaks is stateless (the edit differs from what the
  auto-edit produces for the previous constraints), so it costs one extra
  `generate` per trigger; about 12 ms for 50 clips in a debug build.
- Reverting to "rebuild only on request" means calling `Session::apply`
  instead of `apply_and_regenerate`; nothing else changes.
