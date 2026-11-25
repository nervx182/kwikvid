// Mock backend for the Playwright UI tests (ADR 0011).
//
// The tests serve this file in place of ui/js/api.js (route interception in
// fixtures.js), so the real UI modules run unchanged against it. It exports
// the same functions as api.js and keeps its state on window.__kvMock,
// which the Node-side helpers in fixtures.js drive with page.evaluate():
//
//   state      the Snapshot every call returns (set by mock.state()).
//   beats      what get_beats returns (set by mock.open(s, { beats }) or
//              mock.beats()); null means "no analysis yet".
//   commands   every call the UI made, in order: { command, args? }.
//   responses  queued one-shot answers: { command, snapshot } or
//              { command, error } (set by mock.respond() / mock.fail()).
//              For get_beats and seek, only { command, error } applies.
//              Either may carry delayMs: the answer arrives that much later.
//
// Keep it plain: no imports, no Tauri, nothing a feature would need to edit.
// Features test new commands through apply() and the queues above.

const init = window.__kvMockInit || {};

const mock = {
  state: init.state ?? null,
  beats: init.beats ?? null,
  commands: [],
  responses: [],
  listeners: new Set(),

  /** Replace the state and push it to onState listeners, like kv://state. */
  emit(snapshot) {
    mock.state = snapshot;
    for (const cb of mock.listeners) cb(structuredClone(snapshot));
  },
};
window.__kvMock = mock;

function record(command, args) {
  mock.commands.push(args === undefined ? { command } : { command, args: structuredClone(args) });
  const i = mock.responses.findIndex((r) => r.command === command);
  return i >= 0 ? mock.responses.splice(i, 1)[0] : null;
}

/** A command that returns a value, not a snapshot. */
function query(command, args, value) {
  const r = record(command, args);
  if (r && r.error !== undefined) return Promise.reject({ message: r.error });
  return Promise.resolve(structuredClone(value));
}

function later(r, settle) {
  const ms = (r && r.delayMs) || 0;
  return ms > 0 ? new Promise((res) => setTimeout(res, ms)).then(settle) : settle();
}

function call(command, args) {
  const r = record(command, args);
  return later(r, () => {
    if (r) {
      if (r.error !== undefined) return Promise.reject({ message: r.error });
      mock.state = r.snapshot;
    }
    if (mock.state === null) {
      return Promise.reject({ message: `mock-api: no state set (call mock.open(snapshot))` });
    }
    return Promise.resolve(structuredClone(mock.state));
  });
}

export function apply(cmd) {
  return call("apply", { cmd });
}

export function undo() {
  return call("undo");
}

export function redo() {
  return call("redo");
}

export function reroll(section) {
  return call("reroll", { section });
}

export function getState() {
  return call("get_state");
}

export function getBeats(section) {
  return query("get_beats", { section }, mock.beats);
}

export function seek(position) {
  return query("seek", { position }, Math.max(0, position));
}

export function onState(cb) {
  mock.listeners.add(cb);
  return Promise.resolve(() => mock.listeners.delete(cb));
}
