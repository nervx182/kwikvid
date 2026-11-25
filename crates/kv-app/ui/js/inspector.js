// Clip inspector: opens for the clip selected in the library. A large
// filmstrip with a scrub head (time readout and the nearest filmstrip frame
// above it), HiLight ticks, in/out marks and the clip's pins (green) and
// excludes (red, hatched) as overlays, each with a remove button.
//
// I / O (or the buttons) mark in and out; "Always include" sends
// Command::PinRange and "Exclude" Command::ExcludeRange for the marked range.
// With only one mark (or none: the scrub head), the range is DEFAULT_RANGE
// centred on that point, clamped to the clip. Clicking a HiLight tick pins
// DEFAULT_RANGE around it. The remove (×) buttons send UnpinRange /
// UnexcludeRange with the exact range from the snapshot.
//
// The session rejects a range that overlaps the opposite kind; that message,
// or a regen_error (e.g. pins longer than the target), shows in the
// inspector and the marks stay. A clean success clears the marks.
//
// Scrubbing: drag on the filmstrip, or focus it and use the arrow keys
// (1 s, Shift: 0.1 s), Home and End. The exact frame will come from the
// preview engine; for now the frame is the nearest filmstrip thumbnail.

import * as api from "./api.js";
import { SELECT_EVENT, selectedMedia } from "./library.js";
import { FILMSTRIP_CELLS, formatTime, nearestCell, thumbUrl } from "./timeline.js";

/** Length of a range made from a single point (µs). Quik uses a short default (guess). */
export const DEFAULT_RANGE = 2_000_000;
const STEP = 1_000_000;
const FINE_STEP = 100_000;

const state = {
  render: null,
  snapshot: null,
  mediaId: null,
  head: 0,
  markIn: null,
  markOut: null,
  error: "",
  busy: false,
  dragging: false,
};

/** DEFAULT_RANGE centred on `t`, shifted to stay inside 0..duration. */
export function pointRange(t, duration) {
  const len = Math.min(DEFAULT_RANGE, duration);
  let start = Math.round(t - len / 2);
  start = Math.min(Math.max(0, start), duration - len);
  return { start, end: start + len };
}

/** The range to pin or exclude for the marks (null: unset) and the head. */
export function markedRange(markIn, markOut, head, duration) {
  if (markIn !== null && markOut !== null && markOut > markIn) {
    return { start: markIn, end: markOut };
  }
  const point = markIn ?? markOut ?? head;
  return pointRange(point, duration);
}

function el(tag, className, testid) {
  const e = document.createElement(tag);
  if (className) e.className = className;
  if (testid) e.dataset.testid = testid;
  return e;
}

function button(testid, text, title, onClick) {
  const b = el("button", "", testid);
  b.type = "button";
  b.textContent = text;
  if (title) b.title = title;
  b.addEventListener("click", onClick);
  return b;
}

const $ = (id) => document.getElementById(id);
const pct = (t, d) => `${(d > 0 ? Math.min(1, Math.max(0, t / d)) : 0) * 100}%`;

function section() {
  return state.snapshot ? state.snapshot.project.sections[0] || null : null;
}

function item() {
  if (!state.snapshot || state.mediaId === null) return null;
  return state.snapshot.project.media.find((m) => m.id === state.mediaId) || null;
}

/** Build the inspector DOM and wire it. render(snapshot) renders a command result. */
export function initInspector(render) {
  state.render = render;
  const pane = document.querySelector("[data-testid=inspector]");
  const root = el("div", "ins", "clip-inspector");
  root.id = "clip-inspector";
  root.hidden = true;

  const name = el("div", "ins-name", "inspector-name");
  name.id = "inspector-name";
  const frameBox = el("div", "ins-frame");
  const frame = el("img", "", "inspector-frame");
  frame.id = "inspector-frame";
  frame.alt = "";
  frame.draggable = false;
  frame.addEventListener("error", () => (frame.style.visibility = "hidden"));
  frame.addEventListener("load", () => (frame.style.visibility = ""));
  frameBox.append(frame);

  const strip = el("div", "ins-strip", "inspector-filmstrip");
  strip.id = "inspector-filmstrip";
  strip.tabIndex = 0;
  strip.setAttribute("role", "slider");
  strip.setAttribute("aria-label", "Scrub");
  const cells = el("div", "ins-cells");
  cells.id = "inspector-cells";
  const marked = el("div", "ins-marked", "inspector-marked");
  marked.id = "inspector-marked";
  const markIn = el("div", "ins-mark ins-mark-in", "inspector-in");
  markIn.id = "inspector-in";
  const markOut = el("div", "ins-mark ins-mark-out", "inspector-out");
  markOut.id = "inspector-out";
  const overlays = el("div", "ins-overlays");
  overlays.id = "inspector-overlays";
  const ticks = el("div", "ins-ticks");
  ticks.id = "inspector-ticks";
  const head = el("div", "ins-head", "inspector-head");
  head.id = "inspector-head";
  strip.append(cells, marked, overlays, markIn, markOut, ticks, head);

  const bar = el("div", "ins-bar");
  const time = el("span", "ins-time", "inspector-time");
  time.id = "inspector-time";
  bar.append(
    time,
    button("inspector-mark-in", "In", "Mark in (I)", () => mark("in")),
    button("inspector-mark-out", "Out", "Mark out (O)", () => mark("out")),
    button("inspector-clear", "Clear", "Clear the marks", () => clearMarks()),
  );
  const range = el("div", "ins-range", "inspector-range");
  range.id = "inspector-range";
  const actions = el("div", "ins-actions");
  const include = button("inspector-include", "Always include", "Always use this range in the edit", () =>
    constrain("pin_range"),
  );
  include.id = "inspector-include";
  const exclude = button("inspector-exclude", "Exclude", "Never use this range in the edit", () =>
    constrain("exclude_range"),
  );
  exclude.id = "inspector-exclude";
  actions.append(include, exclude);
  const error = el("p", "ins-error", "inspector-error");
  error.id = "inspector-error";
  error.setAttribute("role", "alert");
  error.hidden = true;

  root.append(name, frameBox, strip, bar, range, actions, error);
  pane.querySelector("h2").after(root);

  strip.addEventListener("pointerdown", (e) => {
    if (e.button !== 0 || e.target.closest("button")) return;
    state.dragging = true;
    strip.setPointerCapture(e.pointerId);
    scrubTo(timeAt(e.clientX));
  });
  strip.addEventListener("pointermove", (e) => {
    if (state.dragging) scrubTo(timeAt(e.clientX));
  });
  const stop = () => (state.dragging = false);
  strip.addEventListener("pointerup", stop);
  strip.addEventListener("pointercancel", stop);
  strip.addEventListener("keydown", (e) => {
    const it = item();
    if (!it || e.metaKey || e.ctrlKey || e.altKey) return;
    const step = e.shiftKey ? FINE_STEP : STEP;
    const moves = {
      ArrowLeft: state.head - step,
      ArrowRight: state.head + step,
      Home: 0,
      End: it.duration,
    };
    if (!(e.key in moves)) return;
    e.preventDefault();
    scrubTo(moves[e.key]);
  });
  document.addEventListener("keydown", (e) => {
    if (root.hidden || e.metaKey || e.ctrlKey || e.altKey || e.repeat) return;
    const t = e.target;
    if (t && (t.isContentEditable || ["INPUT", "TEXTAREA", "SELECT"].includes(t.tagName))) return;
    const k = e.key.toLowerCase();
    if (k !== "i" && k !== "o") return;
    e.preventDefault();
    mark(k === "i" ? "in" : "out");
  });
  document.addEventListener(SELECT_EVENT, (e) => open(e.detail ? e.detail.mediaId : null));
  open(selectedMedia());
}

function timeAt(clientX) {
  const it = item();
  const box = $("inspector-filmstrip").getBoundingClientRect();
  if (!it || box.width <= 0) return 0;
  return ((clientX - box.left) / box.width) * it.duration;
}

function open(mediaId) {
  if (mediaId === state.mediaId) return;
  state.mediaId = mediaId;
  state.head = 0;
  state.markIn = state.markOut = null;
  state.error = "";
  buildStrip();
  draw();
}

function scrubTo(t) {
  const it = item();
  if (!it) return;
  state.head = Math.round(Math.min(Math.max(0, t), it.duration));
  drawHead();
}

function mark(which) {
  if (!item()) return;
  const t = state.head;
  if (which === "in") {
    state.markIn = t;
    if (state.markOut !== null && state.markOut <= t) state.markOut = null;
  } else {
    state.markOut = t;
    if (state.markIn !== null && state.markIn >= t) state.markIn = null;
  }
  state.error = "";
  draw();
}

function clearMarks() {
  state.markIn = state.markOut = null;
  state.error = "";
  draw();
}

function constrain(type, range) {
  const it = item();
  const sec = section();
  if (!it || !sec) return;
  const r = range || markedRange(state.markIn, state.markOut, state.head, it.duration);
  send({ type, section: sec.id, range: { media_id: it.id, start: r.start, end: r.end } }, true);
}

/** Apply a command; show a rejection or regen_error here, keeping the marks. */
async function send(cmd, clearOnSuccess) {
  if (state.busy) return;
  state.busy = true;
  draw();
  try {
    const result = await api.apply(cmd);
    state.error = result.regen_error || "";
    if (clearOnSuccess && !state.error) state.markIn = state.markOut = null;
    state.busy = false;
    state.render(result);
  } catch (e) {
    state.error = e && e.message ? e.message : String(e);
  } finally {
    state.busy = false;
    draw();
  }
}

/** Filmstrip cells for the open clip (rebuilt only when the clip changes). */
function buildStrip() {
  const cells = $("inspector-cells");
  cells.replaceChildren();
  if (state.mediaId === null) return;
  for (let i = 0; i < FILMSTRIP_CELLS; i++) {
    const cell = el("div", "ins-cell");
    cell.dataset.thumb = thumbUrl(state.mediaId, i);
    const img = el("img");
    img.alt = "";
    img.draggable = false;
    img.addEventListener("error", () => img.remove());
    img.src = cell.dataset.thumb;
    cell.append(img);
    cells.append(cell);
  }
}

function drawHead() {
  const it = item();
  if (!it) return;
  const head = $("inspector-head");
  head.style.left = pct(state.head, it.duration);
  head.dataset.time = String(state.head);
  const strip = $("inspector-filmstrip");
  strip.setAttribute("aria-valuemin", "0");
  strip.setAttribute("aria-valuemax", String(it.duration));
  strip.setAttribute("aria-valuenow", String(state.head));
  strip.setAttribute("aria-valuetext", formatTime(state.head));
  $("inspector-time").textContent = `${formatTime(state.head)} / ${formatTime(it.duration)}`;
  const cell = String(nearestCell(state.head, it.duration));
  const frame = $("inspector-frame");
  if (frame.dataset.cell !== cell || frame.dataset.media !== String(it.id)) {
    frame.dataset.cell = cell;
    frame.dataset.media = String(it.id);
    frame.src = thumbUrl(it.id, Number(cell));
  }
}

function overlay(kind, range, duration, label) {
  const o = el("div", `ins-overlay ins-${kind}`, kind === "pin" ? "inspector-pin" : "inspector-excluded");
  o.dataset.start = String(range.start);
  o.dataset.end = String(range.end);
  o.style.left = pct(range.start, duration);
  o.style.width = `calc(${pct(range.end, duration)} - ${pct(range.start, duration)})`;
  o.title = `${label} ${formatTime(range.start)}–${formatTime(range.end)}`;
  const type = kind === "pin" ? "unpin_range" : "unexclude_range";
  const x = button("inspector-remove", "×", `Remove: ${o.title}`, () =>
    send({ type, section: section().id, range: { ...range } }, false),
  );
  x.className = "ins-remove";
  o.append(x);
  return o;
}

function drawMark(id, t, duration) {
  const m = $(id);
  m.hidden = t === null;
  if (t === null) {
    delete m.dataset.time;
    return;
  }
  m.dataset.time = String(t);
  m.style.left = pct(t, duration);
}

function draw() {
  const root = $("clip-inspector");
  const pane = document.querySelector("[data-testid=inspector]");
  const it = item();
  root.hidden = !it;
  pane.toggleAttribute("data-clip", !!it);
  if (!it) return;
  const name = String(it.path).split(/[\\/]/).pop();
  $("inspector-name").textContent = name;
  $("inspector-name").title = name;
  drawHead();

  const sec = section();
  const overlays = $("inspector-overlays");
  overlays.replaceChildren();
  if (sec) {
    for (const r of sec.pins) {
      if (r.media_id === it.id) overlays.append(overlay("pin", r, it.duration, "Always included"));
    }
    for (const r of sec.excludes) {
      if (r.media_id === it.id) overlays.append(overlay("exclude", r, it.duration, "Excluded"));
    }
  }

  const ticks = $("inspector-ticks");
  ticks.replaceChildren();
  for (const h of it.hilights) {
    const t = button("inspector-hilight", "", `HiLight at ${formatTime(h)}: always include this moment`, () => {
      scrubTo(h);
      constrain("pin_range", pointRange(h, it.duration));
    });
    t.className = "ins-hilight";
    t.dataset.time = String(h);
    t.style.left = pct(h, it.duration);
    ticks.append(t);
  }

  drawMark("inspector-in", state.markIn, it.duration);
  drawMark("inspector-out", state.markOut, it.duration);
  const r = markedRange(state.markIn, state.markOut, state.head, it.duration);
  const hasMarks = state.markIn !== null || state.markOut !== null;
  const shade = $("inspector-marked");
  shade.hidden = !hasMarks;
  if (hasMarks) {
    shade.style.left = pct(r.start, it.duration);
    shade.style.width = `calc(${pct(r.end, it.duration)} - ${pct(r.start, it.duration)})`;
  }
  $("inspector-range").textContent = hasMarks
    ? `Range ${formatTime(r.start)}–${formatTime(r.end)}`
    : "Mark in (I) and out (O), or use the head";
  for (const id of ["inspector-include", "inspector-exclude"]) $(id).disabled = state.busy || !sec;

  const error = $("inspector-error");
  error.textContent = state.error;
  error.hidden = !state.error;
}

export function renderInspector(snapshot) {
  state.snapshot = snapshot;
  if (state.mediaId !== null && !item()) {
    // The clip left the project (the library also clears its selection).
    open(null);
    return;
  }
  draw();
}
