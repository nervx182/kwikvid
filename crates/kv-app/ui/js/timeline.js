// Timeline: the current section's edit as proportional blocks over the
// music's beat grid, with a playhead. Manual tweaks (tweaks.js: drag to
// reorder, cut nudges, clip audio) decorate the drawn blocks through
// onTimelineDraw. The transport (transport.js) moves the playhead and owns
// the ruler's scrubbing.
//
// Rendering is plain DOM (absolutely positioned blocks, positions in percent
// of the content width): 200 segments plus a few hundred beat ticks is a
// small DOM, zooming only changes one width, hover titles come for free and
// UI tests can inspect every block. Times are microseconds (kv_core::Micros).

import * as api from "./api.js";

/** Filmstrip cells per clip (the library's filmstrip, served as kvthumb://). */
export const FILMSTRIP_CELLS = 12;
const MAX_ZOOM = 64;

const state = {
  run: null,
  snapshot: null,
  zoom: 1,
  position: 0,
  /** { music, data, revision } for the last get_beats answer. */
  beats: null,
  beatsInFlight: null,
};

const $ = (id) => document.getElementById(id);

/** Callbacks run after every draw (see onTimelineDraw). */
const drawHooks = [];

/**
 * Call cb({ snapshot, section, span }) after every redraw, once the segment
 * blocks (.tl-seg, data-index) are in #timeline-track.
 */
export function onTimelineDraw(cb) {
  drawHooks.push(cb);
}

/** URL of filmstrip cell `index` of a media item (served by the backend). */
export function thumbUrl(mediaId, index) {
  return `kvthumb://${mediaId}/${index}`;
}

/** The filmstrip cell whose centre is nearest to source time `t`. */
export function nearestCell(t, duration, cells = FILMSTRIP_CELLS) {
  if (!(duration > 0)) return 0;
  return Math.min(cells - 1, Math.max(0, Math.floor((t / duration) * cells)));
}

export function timelineLen(seg) {
  return Math.floor(((seg.src_out - seg.src_in) * 100) / Math.max(1, seg.speed));
}

/** "1:05.3" for 65.3 s. */
export function formatTime(us) {
  const tenths = Math.max(0, Math.round(us / 100_000));
  const m = Math.floor(tenths / 600);
  const s = (tenths % 600) / 10;
  return `${m}:${s.toFixed(1).padStart(4, "0")}`;
}

function basename(path) {
  return path.split("/").pop();
}

function targetLength(section, beats) {
  const t = section.target_length;
  if (t.type === "fixed") return t.length;
  if (beats) return beats.duration;
  return section.music ? section.music.duration : 0;
}

function covers(range, seg) {
  return range.media_id === seg.media_id && range.start < seg.src_out && seg.src_in < range.end;
}

/** Wire the controls. run(promise) renders a snapshot or toasts an error. */
export function initTimeline(run) {
  state.run = run;
  $("timeline-zoom-in").addEventListener("click", () => setZoom(state.zoom * 2));
  $("timeline-zoom-out").addEventListener("click", () => setZoom(state.zoom / 2));
  $("timeline-zoom-fit").addEventListener("click", () => setZoom(1));
}

/** The timeline time under page x coordinate `x`, or null when not shown. */
export function timeAtClientX(x) {
  const content = $("timeline-content");
  const rect = content.getBoundingClientRect();
  if (rect.width <= 0) return null;
  const frac = Math.min(1, Math.max(0, (x - rect.left) / rect.width));
  return Math.round(frac * Number(content.dataset.span || 0));
}

/** The beat times of the current music, or null while unknown. */
export function currentBeats() {
  return state.beats && state.beats.data ? state.beats.data.beats : null;
}

function setZoom(z) {
  state.zoom = Math.min(MAX_ZOOM, Math.max(1, z));
  $("timeline-content").style.width = `${state.zoom * 100}%`;
  $("timeline-view").dataset.zoom = String(state.zoom);
  $("timeline-zoom-out").disabled = state.zoom <= 1;
  $("timeline-zoom-in").disabled = state.zoom >= MAX_ZOOM;
}

/** Draw the playhead at `t` (the transport calls it). */
export function setPlayhead(t) {
  state.position = Math.max(0, t);
  const span = Number($("timeline-content").dataset.span || 0);
  const ph = $("timeline-playhead");
  ph.dataset.position = String(state.position);
  ph.style.left = span > 0 ? `${Math.min(100, (state.position / span) * 100)}%` : "0%";
}

function section(snapshot) {
  return snapshot.project.sections[0] || null;
}

/** Ask for the beat grid when the music changed or its analysis was pending. */
function refreshBeats(snapshot, sec) {
  const music = sec && sec.music ? sec.music.path : null;
  if (music === null) {
    state.beats = null;
    return;
  }
  const have = state.beats;
  if (have && have.music === music && (have.data || have.revision === snapshot.revision)) return;
  if (state.beatsInFlight === `${music}@${snapshot.revision}`) return;
  const key = `${music}@${snapshot.revision}`;
  state.beatsInFlight = key;
  api
    .getBeats(sec.id)
    .then((data) => {
      if (state.beatsInFlight === key) state.beatsInFlight = null;
      // Drop answers for music the section no longer has.
      const now = state.snapshot && section(state.snapshot);
      if (!now || !now.music || now.music.path !== music) return;
      if (data && data.music !== music) return;
      state.beats = { music, data: data || null, revision: snapshot.revision };
      draw();
    })
    .catch(() => {
      if (state.beatsInFlight === key) state.beatsInFlight = null;
    });
}

export function renderTimeline(snapshot) {
  state.snapshot = snapshot;
  refreshBeats(snapshot, section(snapshot));
  draw();
}

function draw() {
  const snapshot = state.snapshot;
  if (!snapshot) return;
  const sec = section(snapshot);
  const media = new Map(snapshot.project.media.map((m) => [m.id, m]));
  const segments = sec ? sec.edit.segments : [];
  const beats = state.beats && state.beats.data;

  const total = segments.reduce((end, s) => Math.max(end, s.timeline_start + timelineLen(s)), 0);
  const target = sec ? targetLength(sec, beats) : 0;
  const span = Math.max(total, target, 1);

  // Header: lengths.
  $("timeline-length").textContent = formatTime(total);
  $("timeline-target").textContent = target > 0 ? formatTime(target) : "–";

  // Messages: empty states and the last regenerate error.
  let empty = "";
  if (segments.length === 0) {
    if (snapshot.project.media.length === 0) empty = "Import clips to start an edit.";
    else if (!sec || !sec.music) empty = "Add music to create an edit.";
  }
  const emptyEl = $("timeline-empty");
  emptyEl.textContent = empty;
  emptyEl.hidden = empty === "";
  const errEl = $("timeline-error");
  errEl.textContent = snapshot.regen_error || "";
  errEl.hidden = !snapshot.regen_error;

  const content = $("timeline-content");
  content.dataset.span = String(span);
  content.hidden = segments.length === 0 && !beats;

  // Beat grid.
  const ruler = $("timeline-beats");
  const ticks = [];
  if (beats) {
    const down = new Set(beats.downbeats);
    for (const b of beats.beats) {
      if (b < 0 || b > span) continue;
      const tick = document.createElement("div");
      tick.className = down.has(b) ? "tl-beat tl-downbeat" : "tl-beat";
      tick.dataset.t = String(b);
      tick.style.left = `${(b / span) * 100}%`;
      ticks.push(tick);
    }
  }
  ruler.replaceChildren(...ticks);
  const target_marker = $("timeline-target-marker");
  target_marker.hidden = !(target > 0);
  target_marker.style.left = `${(target / span) * 100}%`;

  // Segments.
  const blocks = segments.map((seg, i) => {
    const len = timelineLen(seg);
    const item = media.get(seg.media_id);
    const el = document.createElement("div");
    el.className = "tl-seg";
    el.dataset.index = String(i);
    el.dataset.start = String(seg.timeline_start);
    el.dataset.len = String(len);
    el.style.left = `${(seg.timeline_start / span) * 100}%`;
    el.style.width = `${(len / span) * 100}%`;
    el.title = item ? basename(item.path) : `Missing clip ${seg.media_id}`;

    if (item) {
      const img = document.createElement("img");
      img.className = "tl-thumb";
      img.alt = "";
      img.draggable = false;
      img.loading = "lazy";
      img.addEventListener("error", () => img.remove());
      img.src = thumbUrl(item.id, nearestCell((seg.src_in + seg.src_out) / 2, item.duration));
      el.append(img);
    }
    if (seg.transition.type === "crossfade" && len > 0) {
      const w = document.createElement("div");
      w.className = "tl-xfade";
      w.title = `Crossfade ${formatTime(seg.transition.duration)}`;
      w.style.width = `${Math.min(100, (seg.transition.duration / len) * 100)}%`;
      el.append(w);
      el.dataset.crossfade = String(seg.transition.duration);
    }
    if (seg.speed < 100) {
      const b = document.createElement("span");
      b.className = "tl-badge";
      b.textContent = `${seg.speed / 100}x`;
      el.append(b);
      el.dataset.slowmo = "true";
    }
    if (sec.pins.some((p) => covers(p, seg))) {
      const p = document.createElement("span");
      p.className = "tl-pin";
      p.title = "Pinned";
      p.textContent = "📌";
      el.append(p);
      el.dataset.pinned = "true";
    }
    return el;
  });
  $("timeline-track").replaceChildren(...blocks);

  setPlayhead(state.position);
  for (const cb of drawHooks) cb({ snapshot, section: sec, span });
}
