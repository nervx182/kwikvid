// Manual tweaks on the timeline (MVP item 8): drag a segment to reorder it,
// nudge a selected cut one beat earlier or later, toggle a clip's own audio,
// and set the music volume.
//
//   Drag a segment       MoveSegment { from, to } on release; a drop
//                        indicator shows where it lands; Esc cancels.
//   Click a cut          selects it (the boundary handle between two
//                        segments) and shows "‹ beat" / "beat ›"; the keys
//                        , and . do the same. Esc deselects. The backend
//                        computes the target time from the beat grid
//                        (kv-session nudge_cut).
//   Right-click segment  menu with "Clip audio" -> SetClipAudio; segments
//                        whose clip has audio on show a speaker icon.
//   Music volume         slider 0-100 in the toolbar -> SetMusicVolume,
//                        sent once on release (one undo step per drag).
//
// None of these rebuild the edit (ADR 0010); the hint says what does.
// Times are microseconds (kv_core::Micros).

import * as api from "./api.js";
import { onTimelineDraw } from "./timeline.js";

// The keys the handler below acts on, for the shortcuts overview (shortcuts.js).
// Notation: Tauri accelerators (CmdOrCtrl = Cmd on macOS, Ctrl elsewhere).
export const KEYS = [
  { group: "Timeline", keys: [","], description: "Move the selected cut one beat earlier" },
  { group: "Timeline", keys: ["."], description: "Move the selected cut one beat later" },
  { group: "Timeline", keys: ["Escape"], description: "Cancel a drag, close the clip menu or deselect the cut" },
];

/** Pointer travel (px) before a press on a segment becomes a drag. */
export const DRAG_THRESHOLD = 4;

export const HINT =
  "Drag clips to reorder them, click a cut to move it by a beat. " +
  "Changing the length, forced moments or re-roll rebuilds the edit and replaces these tweaks.";

const state = {
  run: null,
  snapshot: null,
  section: null,
  /** Index of the selected cut (1..segments-1), or null. */
  cut: null,
  /** { from, pointerId, startX, active, to } while a segment is pressed. */
  drag: null,
  volumeDragging: false,
};

const $ = (id) => document.getElementById(id);

function el(tag, className, testid) {
  const e = document.createElement(tag);
  if (className) e.className = className;
  if (testid) e.dataset.testid = testid;
  return e;
}

function button(testid, text, title, onClick) {
  const b = el("button", "", testid);
  b.type = "button";
  b.id = testid;
  b.textContent = text;
  b.title = title;
  b.addEventListener("click", onClick);
  return b;
}

function typing(t) {
  return t && (t.isContentEditable || ["INPUT", "TEXTAREA", "SELECT"].includes(t.tagName));
}

/**
 * The MoveSegment target index for dropping segment `from` into gap `gap`
 * (0 = before the first segment, n = after the last), or null for no move.
 */
export function dropIndex(from, gap) {
  const to = gap > from ? gap - 1 : gap;
  return to === from ? null : to;
}

// ---- Build -----------------------------------------------------------------

function build() {
  // Toolbar: music volume.
  const vol = el("label", "tw-volume", "tweaks-volume-control");
  vol.title = "Music volume (applied when you let go)";
  const name = el("span", "tw-volume-name");
  name.textContent = "Music";
  const slider = el("input", "", "tweaks-volume");
  slider.id = "tweaks-volume";
  slider.type = "range";
  slider.min = "0";
  slider.max = "100";
  slider.step = "1";
  slider.setAttribute("aria-label", "Music volume");
  const value = el("span", "tw-volume-value", "tweaks-volume-value");
  value.id = "tweaks-volume-value";
  vol.append(name, slider, value);
  document.querySelector(".toolbar").append(vol);

  slider.addEventListener("pointerdown", () => (state.volumeDragging = true));
  slider.addEventListener("input", () => {
    value.textContent = `${slider.value}%`;
  });
  slider.addEventListener("change", () => {
    state.volumeDragging = false;
    commitVolume(Number(slider.value));
  });
  // The release comes before the change event; the slider keeps its value
  // until change commits it (a release without movement sends nothing).
  window.addEventListener("pointerup", () => (state.volumeDragging = false));

  // Timeline bar: cut nudge buttons.
  const nudge = el("div", "tw-nudge", "tweaks-nudge");
  nudge.id = "tweaks-nudge";
  nudge.hidden = true;
  const label = el("span", "tw-nudge-label", "tweaks-cut-label");
  label.id = "tweaks-cut-label";
  nudge.append(
    label,
    button("tweaks-nudge-earlier", "‹ beat", "Move the cut one beat earlier (,)", () => nudgeCut(-1)),
    button("tweaks-nudge-later", "beat ›", "Move the cut one beat later (.)", () => nudgeCut(1)),
  );
  document.querySelector(".tl-lengths").after(nudge);

  const hint = el("p", "tw-hint", "tweaks-hint");
  hint.id = "tweaks-hint";
  hint.textContent = HINT;
  hint.hidden = true;
  $("timeline-error").after(hint);

  // Timeline content: cut handles and the drop indicator.
  const cuts = el("div", "tl-cuts", "timeline-cuts");
  cuts.id = "timeline-cuts";
  const drop = el("div", "tl-drop", "timeline-drop");
  drop.id = "timeline-drop";
  drop.hidden = true;
  $("timeline-track").after(cuts, drop);

  cuts.addEventListener("click", (e) => {
    const h = e.target.closest(".tl-cut");
    if (!h) return;
    const cut = Number(h.dataset.cut);
    selectCut(state.cut === cut ? null : cut);
  });

  const track = $("timeline-track");
  track.addEventListener("pointerdown", onPointerDown);
  window.addEventListener("pointermove", onPointerMove);
  window.addEventListener("pointerup", onPointerUp);
  window.addEventListener("pointercancel", () => endDrag());
  track.addEventListener("contextmenu", (e) => {
    const seg = e.target.closest(".tl-seg");
    if (!seg) return;
    e.preventDefault();
    openMenu(Number(seg.dataset.index), e.clientX, e.clientY);
  });
  document.addEventListener("click", (e) => {
    if (!e.target.closest(".tw-menu")) closeMenu();
  });
  document.addEventListener("keydown", onKey);
}

// ---- Drag to reorder -------------------------------------------------------

function blocks() {
  return [...$("timeline-track").querySelectorAll(".tl-seg")];
}

/** The gap under page x: how many segment midpoints lie left of it. */
function gapAt(x) {
  return blocks().filter((b) => {
    const r = b.getBoundingClientRect();
    return r.left + r.width / 2 < x;
  }).length;
}

function onPointerDown(e) {
  const seg = e.target.closest(".tl-seg");
  if (!seg || e.button !== 0) return;
  closeMenu();
  state.drag = {
    from: Number(seg.dataset.index),
    pointerId: e.pointerId,
    startX: e.clientX,
    active: false,
    to: null,
  };
}

function onPointerMove(e) {
  const d = state.drag;
  if (!d || e.pointerId !== d.pointerId) return;
  if (!d.active) {
    if (Math.abs(e.clientX - d.startX) < DRAG_THRESHOLD) return;
    d.active = true;
    const seg = blocks()[d.from];
    if (seg) seg.classList.add("tl-dragging");
    $("timeline-view").dataset.dragging = "true";
  }
  const gap = gapAt(e.clientX);
  d.to = dropIndex(d.from, gap);
  showDrop(d.to === null ? null : gap);
}

function onPointerUp(e) {
  const d = state.drag;
  if (!d || e.pointerId !== d.pointerId) return;
  const sec = state.section;
  endDrag();
  if (d.active && d.to !== null && sec) {
    state.run(api.apply({ type: "move_segment", section: sec.id, from: d.from, to: d.to }));
  }
}

/** Stop dragging without moving anything. */
function endDrag() {
  state.drag = null;
  showDrop(null);
  for (const b of blocks()) b.classList.remove("tl-dragging");
  delete $("timeline-view").dataset.dragging;
}

function showDrop(gap) {
  const drop = $("timeline-drop");
  const bs = blocks();
  if (gap === null || bs.length === 0) {
    drop.hidden = true;
    delete drop.dataset.gap;
    return;
  }
  const content = $("timeline-content").getBoundingClientRect();
  const x =
    gap < bs.length ? bs[gap].getBoundingClientRect().left : bs[bs.length - 1].getBoundingClientRect().right;
  drop.style.left = `${content.width > 0 ? ((x - content.left) / content.width) * 100 : 0}%`;
  drop.dataset.gap = String(gap);
  drop.hidden = false;
}

// ---- Cuts ------------------------------------------------------------------

function selectCut(cut) {
  state.cut = cut;
  drawCuts();
}

function nudgeCut(beats) {
  const sec = state.section;
  if (!sec || state.cut === null || !sec.music) return;
  state.run(api.invoke("nudge_cut", { section: sec.id, cut: state.cut, beats }));
}

function drawCuts(span) {
  const layer = $("timeline-cuts");
  if (span !== undefined) layer.dataset.span = String(span);
  const total = Number(layer.dataset.span || 0);
  const segs = state.section ? state.section.edit.segments : [];
  if (state.cut !== null && state.cut >= segs.length) state.cut = null;
  const handles = [];
  for (let i = 1; i < segs.length; i++) {
    const h = el("button", "tl-cut", "timeline-cut");
    h.type = "button";
    h.dataset.cut = String(i);
    h.dataset.t = String(segs[i].timeline_start);
    h.style.left = `${total > 0 ? (segs[i].timeline_start / total) * 100 : 0}%`;
    h.title = "Select this cut to move it by a beat";
    h.setAttribute("aria-label", `Cut ${i}`);
    h.setAttribute("aria-pressed", String(state.cut === i));
    handles.push(h);
  }
  layer.replaceChildren(...handles);

  const nudge = $("tweaks-nudge");
  nudge.hidden = state.cut === null;
  if (state.cut !== null) {
    const music = !!(state.section && state.section.music);
    $("tweaks-cut-label").textContent = `Cut ${state.cut}`;
    for (const id of ["tweaks-nudge-earlier", "tweaks-nudge-later"]) {
      $(id).disabled = !music;
    }
    nudge.title = music ? "" : "Add music to move cuts to the beat.";
  }
}

// ---- Clip audio ------------------------------------------------------------

function mediaOf(index) {
  const sec = state.section;
  const seg = sec && sec.edit.segments[index];
  if (!seg) return null;
  return state.snapshot.project.media.find((m) => m.id === seg.media_id) || null;
}

function openMenu(index, x, y) {
  closeMenu();
  const item = mediaOf(index);
  if (!item) return;
  const menu = el("div", "tw-menu", "timeline-menu");
  menu.setAttribute("role", "menu");
  const b = el("button", "", "tweaks-clip-audio");
  b.type = "button";
  b.setAttribute("role", "menuitemcheckbox");
  b.setAttribute("aria-checked", String(!!item.clip_audio));
  b.textContent = `${item.clip_audio ? "✓ " : ""}Clip audio`;
  b.addEventListener("click", () => {
    closeMenu();
    state.run(api.apply({ type: "set_clip_audio", media_id: item.id, enabled: !item.clip_audio }));
  });
  menu.append(b);
  menu.style.left = `${x}px`;
  menu.style.top = `${y}px`;
  document.body.append(menu);
}

function closeMenu() {
  for (const m of document.querySelectorAll(".tw-menu")) m.remove();
}

function drawAudioIcons() {
  const media = new Map(state.snapshot.project.media.map((m) => [m.id, m]));
  const segs = state.section ? state.section.edit.segments : [];
  for (const b of blocks()) {
    const seg = segs[Number(b.dataset.index)];
    const item = seg && media.get(seg.media_id);
    if (!item || !item.clip_audio) continue;
    const icon = el("span", "tl-audio", "timeline-clip-audio");
    icon.title = "Clip audio on";
    icon.textContent = "🔊";
    b.append(icon);
    b.dataset.clipAudio = "true";
  }
}

// ---- Music volume ----------------------------------------------------------

function volume() {
  const settings = state.snapshot && state.snapshot.project.settings;
  return settings ? settings.music_volume_percent : 100;
}

function commitVolume(percent) {
  if (!state.snapshot || percent === volume()) return;
  state.run(api.apply({ type: "set_music_volume", percent }));
}

function renderVolume() {
  if (state.volumeDragging || !state.snapshot) return;
  const slider = $("tweaks-volume");
  slider.value = String(volume());
  slider.disabled = !(state.section && state.section.music);
  $("tweaks-volume-value").textContent = `${volume()}%`;
}

// ---- Keys and wiring -------------------------------------------------------

function onKey(e) {
  if (e.key === "Escape") {
    if (state.drag) endDrag();
    else if (document.querySelector(".tw-menu")) closeMenu();
    else if (state.cut !== null) selectCut(null);
    else return;
    return;
  }
  if (state.cut === null || e.defaultPrevented || typing(e.target)) return;
  if (e.metaKey || e.ctrlKey || e.altKey) return;
  if (e.key !== "," && e.key !== ".") return;
  if (document.querySelector('[role="alertdialog"], [role="dialog"], dialog[open]')) return;
  e.preventDefault();
  nudgeCut(e.key === "," ? -1 : 1);
}

function onDraw({ snapshot, section, span }) {
  state.snapshot = snapshot;
  state.section = section;
  // A redraw mid-drag (a new snapshot) invalidates the indices.
  if (state.drag) endDrag();
  drawAudioIcons();
  drawCuts(span);
  $("tweaks-hint").hidden = !(section && section.edit.segments.length > 0);
  renderVolume();
}

/** Wire the tweaks. run(promise) renders a snapshot or toasts an error. */
export function initTweaks(run) {
  state.run = run;
  build();
  onTimelineDraw(onDraw);
}
