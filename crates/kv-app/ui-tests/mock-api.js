// Mock backend for the Playwright UI tests (ADR 0011).
//
// The tests serve this file in place of ui/js/api.js (route interception in
// fixtures.js), so the real UI modules run unchanged against it. It exports
// the same five functions as api.js and keeps its state on window.__kvMock,
// which the Node-side helpers in fixtures.js drive with page.evaluate():
//
//   state      the Snapshot every call returns (set by mock.state()).
//   commands   every call the UI made, in order: { command, args? }.
//   responses  queued one-shot answers: { command, snapshot } or
//              { command, error } (set by mock.respond() / mock.fail()).
//
// Keep it plain: no imports, no Tauri, nothing a feature would need to edit.
// Features test new commands through apply() and the queues above.

const init = window.__kvMockInit || {};

const mock = {
  state: init.state ?? null,
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

function call(command, args) {
  mock.commands.push(args === undefined ? { command } : { command, args: structuredClone(args) });
  const i = mock.responses.findIndex((r) => r.command === command);
  if (i >= 0) {
    const [r] = mock.responses.splice(i, 1);
    if (r.error !== undefined) return Promise.reject({ message: r.error });
    mock.state = r.snapshot;
  }
  if (mock.state === null) {
    return Promise.reject({ message: `mock-api: no state set (call mock.open(snapshot))` });
  }
  return Promise.resolve(structuredClone(mock.state));
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

export function getState() {
  return call("get_state");
}

export function onState(cb) {
  mock.listeners.add(cb);
  return Promise.resolve(() => mock.listeners.delete(cb));
}
