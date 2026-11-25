// Transport: play/pause, scrubbing on the timeline ruler, keyboard stepping
// and the m:ss.ff readout, driving the preview engine (src/transport.rs).
//
// Keys (ignored while typing or while a dialog is open):
//   Space              play / pause
//   Left / Right       one frame back / forward (pauses first)
//   Shift+Left/Right   previous / next beat
//   Home / End         start / end of the edit
//   Cmd/Ctrl+Shift+D   debug overlay: seek latency (p50/p95, also logged
//                      to the console) and frame counters
//
// Scrubbing seeks on every pointer move but keeps at most one seek in
// flight: moves during a seek only remember the newest target, which is
// sent when the answer arrives. The playhead follows the pointer at once
// and follows kv://position events (~15 Hz from the engine) otherwise.
// Times are microseconds.

import * as api from "./api.js";
import { currentBeats, setPlayhead, timeAtClientX, timelineLen } from "./timeline.js";

export const POSITION_EVENT = "kv://position";
const DEBUG_POLL_MS = 500;
/** A beat this close to the playhead counts as "here" (1 ms). */
const BEAT_EPSILON = 1000;

const state = {
  run: null,
  position: 0,
  playing: false,
  frameUs: 1_000_000 / 30,
  end: 0,
  scrubbing: false,
  seekInFlight: false,
  /** Newest target asked for while a seek was in flight. */
  pendingSeek: null,
  debugTimer: 0,
  lastLoggedCount: -1,
};

const $ = (id) => document.getElementById(id);

/** "1:05.12" for 65 s and 12 frames. */
export function formatTimecode(us, frameUs) {
  const fps = Math.max(1, Math.round(1_000_000 / frameUs));
  const k = Math.floor((Math.max(0, us) + 0.5) / frameUs);
  const secs = Math.floor(k / fps);
  const ff = k % fps;
  const m = Math.floor(secs / 60);
  const ss = secs % 60;
  return `${m}:${String(ss).padStart(2, "0")}.${String(ff).padStart(2, "0")}`;
}

/** The frame grid index at `us` (frame times are rounded to the microsecond). */
function frameIndex(us) {
  return Math.floor((Math.max(0, us) + 0.5) / state.frameUs);
}

function toast(e) {
  // The shared error path, without touching the snapshot.
  state.run(Promise.reject(e));
}

function busySeeking() {
  return state.scrubbing || state.seekInFlight || state.pendingSeek !== null;
}

function showPosition(t) {
  state.position = Math.max(0, t);
  setPlayhead(state.position);
  const time = $("transport-time");
  time.textContent = formatTimecode(state.position, state.frameUs);
  time.dataset.position = String(state.position);
}

function showPlaying(playing) {
  state.playing = playing;
  const b = $("transport-play");
  b.dataset.playing = String(playing);
  b.textContent = playing ? "❚❚" : "▶";
  b.title = playing ? "Pause (Space)" : "Play (Space)";
  b.setAttribute("aria-label", playing ? "Pause" : "Play");
}

/** Take a TransportStatus from the backend (command answer or event). */
function applyStatus(s) {
  if (!s || typeof s.position !== "number") return;
  if (typeof s.frame_us === "number" && s.frame_us > 0) state.frameUs = s.frame_us;
  showPlaying(!!s.playing);
  if (!busySeeking()) showPosition(s.position);
}

function sendSeek(t) {
  state.seekInFlight = true;
  api
    .seek(t)
    .then((s) => {
      state.seekInFlight = false;
      if (state.pendingSeek === null && !state.scrubbing) applyStatus(s);
    })
    .catch((e) => {
      state.seekInFlight = false;
      toast(e);
    })
    .finally(() => {
      if (state.pendingSeek !== null) {
        const next = state.pendingSeek;
        state.pendingSeek = null;
        sendSeek(next);
      }
    });
}

/** Move the playhead to `t` now and tell the backend (one seek in flight). */
export function seek(t) {
  const target = Math.max(0, Math.round(t));
  showPosition(target);
  if (state.seekInFlight) {
    state.pendingSeek = target;
    return;
  }
  sendSeek(target);
}

export function play() {
  api.play().then(applyStatus, toast);
}

export function pause() {
  api.pause().then(applyStatus, toast);
}

export function togglePlay() {
  if (state.playing) pause();
  else play();
}

/** One frame forward (+1) or back (-1); pauses first. */
export function stepFrame(dir) {
  if (state.playing) pause();
  const k = Math.max(0, frameIndex(state.position) + dir);
  seek(Math.round(k * state.frameUs));
}

/** To the next (+1) or previous (-1) beat of the music, if known. */
export function stepBeat(dir) {
  const beats = currentBeats();
  if (!beats || beats.length === 0) return;
  const p = state.position;
  const target =
    dir > 0
      ? beats.find((b) => b > p + BEAT_EPSILON)
      : [...beats].reverse().find((b) => b < p - BEAT_EPSILON);
  if (target !== undefined) seek(target);
}

function typing(t) {
  return t && (t.isContentEditable || ["INPUT", "TEXTAREA", "SELECT"].includes(t.tagName));
}

function onKey(e) {
  if (e.defaultPrevented || typing(e.target)) return;
  if (document.querySelector('[role="alertdialog"], [role="dialog"], dialog[open]')) return;
  if ((e.metaKey || e.ctrlKey) && e.shiftKey && !e.altKey && e.code === "KeyD") {
    e.preventDefault();
    toggleDebug();
    return;
  }
  if (e.metaKey || e.ctrlKey || e.altKey) return;
  switch (e.key) {
    case " ":
      if (e.repeat) break;
      togglePlay();
      break;
    case "ArrowRight":
      if (e.shiftKey) stepBeat(1);
      else stepFrame(1);
      break;
    case "ArrowLeft":
      if (e.shiftKey) stepBeat(-1);
      else stepFrame(-1);
      break;
    case "Home":
      seek(0);
      break;
    case "End":
      seek(state.end);
      break;
    default:
      return;
  }
  // Also keeps Space from pressing a focused button a second time.
  e.preventDefault();
}

function initScrub() {
  const ruler = $("timeline-ruler");
  const at = (e) => timeAtClientX(e.clientX);
  ruler.addEventListener("pointerdown", (e) => {
    if (e.button !== 0) return;
    const t = at(e);
    if (t === null) return;
    state.scrubbing = true;
    try {
      ruler.setPointerCapture(e.pointerId);
    } catch {
      // Synthetic pointers cannot be captured; moves still arrive.
    }
    seek(t);
  });
  ruler.addEventListener("pointermove", (e) => {
    if (!state.scrubbing) return;
    const t = at(e);
    if (t !== null) seek(t);
  });
  const end = () => {
    state.scrubbing = false;
  };
  ruler.addEventListener("pointerup", end);
  ruler.addEventListener("pointercancel", end);
}

// ---- Debug overlay ---------------------------------------------------------

function fmtMs(v) {
  return v === null || v === undefined ? "–" : `${v.toFixed(1)} ms`;
}

function renderDebug(d) {
  const el = $("preview-debug");
  const s = d.seek || {};
  el.textContent =
    `preview ${d.backend}${d.engine ? "" : " (no engine)"} · ` +
    `seek last ${fmtMs(s.last_ms)} p50 ${fmtMs(s.p50_ms)} p95 ${fmtMs(s.p95_ms)} (n=${s.count || 0}) · ` +
    `frames ${d.presented} shown, ${d.dropped} dropped`;
  if (s.count > 0 && s.count !== state.lastLoggedCount) {
    state.lastLoggedCount = s.count;
    console.info(
      `kwikvid seek latency: p50 ${fmtMs(s.p50_ms)}, p95 ${fmtMs(s.p95_ms)} over ${s.count} seeks`,
    );
  }
}

function pollDebug() {
  api.getPreviewStats().then(renderDebug, (e) => console.warn("get_preview_stats failed", e));
}

export function toggleDebug() {
  const el = $("preview-debug");
  el.hidden = !el.hidden;
  if (state.debugTimer) {
    clearInterval(state.debugTimer);
    state.debugTimer = 0;
  }
  if (!el.hidden) {
    pollDebug();
    state.debugTimer = setInterval(pollDebug, DEBUG_POLL_MS);
  }
}

// ---- Wiring ----------------------------------------------------------------

/** Wire the controls. run(promise) renders a snapshot or toasts an error. */
export function initTransport(run) {
  state.run = run;
  $("transport-play").addEventListener("click", togglePlay);
  document.addEventListener("keydown", onKey);
  initScrub();
  api.onEvent(POSITION_EVENT, applyStatus);
  api.getTransport().then(applyStatus, () => {});
  showPlaying(false);
  showPosition(0);
}

/** The edit's length, for End and the readout. */
export function renderTransport(snapshot) {
  const sec = snapshot.project.sections[0];
  const segments = sec ? sec.edit.segments : [];
  state.end = segments.reduce((end, s) => Math.max(end, s.timeline_start + timelineLen(s)), 0);
  const len = $("transport-length");
  len.textContent = formatTimecode(state.end, state.frameUs);
  len.dataset.length = String(state.end);
}
