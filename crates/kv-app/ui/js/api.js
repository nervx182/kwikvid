// The only module that touches window.__TAURI__. UI tests replace this file
// with a mock exporting the same functions.
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
