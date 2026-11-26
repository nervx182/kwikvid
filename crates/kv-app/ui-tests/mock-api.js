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
//   responses  queued one-shot answers: { command, snapshot },
//              { command, value } or { command, error } (set by
//              mock.respond() / mock.reply() / mock.fail()).
//              For get_beats, seek and the other transport commands,
//              only { command, error } applies (with delayMs for all).
//              Either may carry delayMs: the answer arrives that much later.
//   transport  { position, playing, frame_us }: what play, pause, seek and
//              get_transport answer; play/pause/seek update it.
//   stats      what get_preview_stats answers (set via __kvMockInit.stats).
//   values     standing answers for commands that do not return a
//              Snapshot: { [command]: value } (set by mock.open(s, { values })).
//              values.get_preview_backend picks the preview backend.
//   fire(name, payload)  deliver a backend event to onEvent listeners
//              (set by mock.event()).
//
// Keep it plain: no imports, no Tauri, nothing a feature would need to edit.
// Features test new commands through apply() and the queues above.

const init = window.__kvMockInit || {};

const mock = {
  state: init.state ?? null,
  beats: init.beats ?? null,
  commands: [],
  responses: [],
  values: init.values ?? {},
  transport: { position: 0, playing: false, frame_us: 1_000_000 / 30 },
  stats: init.stats ?? {
    backend: "off",
    engine: false,
    presented: 0,
    dropped: 0,
    seek: { count: 0, last_ms: null, p50_ms: null, p95_ms: null },
  },
  listeners: new Set(),
  eventListeners: [],

  /** Replace the state and push it to onState listeners, like kv://state. */
  emit(snapshot) {
    mock.state = snapshot;
    for (const cb of mock.listeners) cb(structuredClone(snapshot));
  },

  /** Deliver a backend event, like Tauri's event system. */
  fire(name, payload) {
    for (const l of mock.eventListeners) if (l.name === name) l.cb(structuredClone(payload));
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
  return later(r, () => {
    if (r && r.error !== undefined) return Promise.reject({ message: r.error });
    return Promise.resolve(structuredClone(typeof value === "function" ? value() : value));
  });
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
      if ("value" in r) return Promise.resolve(structuredClone(r.value));
      mock.state = r.snapshot;
    } else if (command in mock.values) {
      return Promise.resolve(structuredClone(mock.values[command]));
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
  return query("seek", { position }, () => {
    mock.transport.position = Math.max(0, position);
    return mock.transport;
  });
}

export function play() {
  return query("play", undefined, () => {
    mock.transport.playing = true;
    return mock.transport;
  });
}

export function pause() {
  return query("pause", undefined, () => {
    mock.transport.playing = false;
    return mock.transport;
  });
}

export function getTransport() {
  return query("get_transport", undefined, () => mock.transport);
}

export function getPreviewStats() {
  return query("get_preview_stats", undefined, () => mock.stats);
}

// The viewer rectangle for the native preview layer; resolves to null like
// the real command.
export function setViewerRect(rect) {
  return query("set_viewer_rect", rect, null);
}

// "off" unless a test sets values.get_preview_backend (e.g. "webgl").
export function getPreviewBackend() {
  return query("get_preview_backend", undefined, () => mock.values.get_preview_backend ?? "off");
}

// Preview frames: tests answer these with page.route("http://kvframe.localhost/**").
export function frameUrl(after) {
  return `http://kvframe.localhost/latest?after=${after}`;
}

export function onState(cb) {
  mock.listeners.add(cb);
  return Promise.resolve(() => mock.listeners.delete(cb));
}

export function invoke(cmd, args) {
  return call(cmd, args);
}

export function onEvent(name, cb) {
  const l = { name, cb };
  mock.eventListeners.push(l);
  return Promise.resolve(() => {
    mock.eventListeners = mock.eventListeners.filter((x) => x !== l);
  });
}
