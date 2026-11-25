// The only module that touches window.__TAURI__. UI tests replace this file
// with a mock (ui-tests/mock-api.js) exporting the same functions.
//
// Every function returns a Promise. Failures reject with { message }, a short
// sentence fit for a toast.

const tauri = window.__TAURI__;

const STATE_EVENT = "kv://state";

function asError(e) {
  if (e && typeof e.message === "string") return { message: e.message };
  return { message: String(e) };
}

async function call(cmd, args) {
  try {
    return await tauri.core.invoke(cmd, args);
  } catch (e) {
    throw asError(e);
  }
}

/** Apply a kv_core::Command, e.g. { type: "set_seed", section: 1, seed: 7 }. */
export function apply(cmd) {
  return call("apply", { cmd });
}

export function undo() {
  return call("undo");
}

export function redo() {
  return call("redo");
}

/**
 * The beat grid of a section's music: { music, duration, tempo_bpm, beats,
 * downbeats } (microseconds), or null while there is no music or its
 * analysis has not finished.
 */
export function getBeats(section) {
  return call("get_beats", { section });
}

/** Move the playhead to `position` (microseconds); resolves to the position used. */
export function seek(position) {
  return call("seek", { position });
}

/**
 * Re-roll a section's auto-edit (next seed, pins kept). Resolves to the new
 * snapshot (with manual_edits_replaced); rejects when the edit cannot be
 * built (no clips, no music, ...), changing nothing.
 */
export function reroll(section) {
  return call("reroll", { section });
}

/** The current Snapshot (snake_case keys, see kv-session). */
export function getState() {
  return call("get_state");
}

/**
 * Report the viewer rectangle so the native preview layer can cover it:
 * { x, y, w, h } in CSS pixels relative to the page, scale = devicePixelRatio.
 */
export function setViewerRect(rect) {
  return call("set_viewer_rect", rect);
}

/**
 * Call cb(snapshot) for every state the backend emits. Resolves to a function
 * that stops listening.
 */
export function onState(cb) {
  return tauri.event.listen(STATE_EVENT, (event) => cb(event.payload));
}

/**
 * Call any other backend command by name, e.g. invoke("open_project",
 * { path }). Resolves to whatever the command returns.
 */
export function invoke(cmd, args) {
  return call(cmd, args);
}

/**
 * Call cb(payload) for every backend event `name` (e.g. "kv://menu").
 * Resolves to a function that stops listening.
 */
export function onEvent(name, cb) {
  return tauri.event.listen(name, (event) => cb(event.payload));
}
