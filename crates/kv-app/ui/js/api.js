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
