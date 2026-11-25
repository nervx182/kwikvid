// Music picker: "Choose file…" (MP3/AAC/WAV/FLAC) or a bundled track, the
// analysis status while it runs, then the section's song with its name,
// duration, tempo and a beat strip over the first 30 s, and "Remove music".
//
// Backend (src/music.rs, ADR 0013): invoke("bundled_tracks") resolves to
// [{ id, title, artist, path }] (empty when none ship); invoke("pick_music")
// to an absolute path or null; invoke("analyze_music", { section, path }) to
// the queued job { id, kind: "music-analysis", state, ... }. The job then
// reports on kv://job; when it is "done" the backend has already set the
// music (Command::SetMusic) and emitted kv://state; "failed" carries the
// message and the music is unchanged. Tempo and beats come from getBeats.
// Removing sends Command::SetMusic { music: null } through apply.

import * as api from "./api.js";
import { showToast } from "./toast.js";
import { formatDuration } from "./library.js";

/** The beat strip covers this much of the song (microseconds). */
export const STRIP_SPAN = 30_000_000;

const KIND = "music-analysis";
const FINAL = new Set(["done", "failed", "cancelled"]);

const state = {
  run: null,
  snapshot: null,
  /** { id, name, fraction } of the analysis being waited for. */
  pending: null,
  /** Final music-analysis updates seen before their invoke resolved. */
  early: new Map(),
  error: null,
  /** The getBeats answer for the current music path, or null. */
  beats: null,
  beatsFor: null,
  bundled: [],
};

/** Left offset of a beat tick in the strip, as a fraction (null: past the span). */
export function stripFraction(t) {
  if (!(t >= 0) || t > STRIP_SPAN) return null;
  return t / STRIP_SPAN;
}

function basename(path) {
  return String(path).split(/[\\/]/).pop();
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
  b.title = title;
  b.addEventListener("click", onClick);
  return b;
}

const $ = (testid) => document.querySelector(`[data-testid="${testid}"]`);

function section() {
  const s = state.snapshot;
  return s && s.project.sections.length > 0 ? s.project.sections[0] : null;
}

/** Build the panel (at the bottom of the library pane) and wire events. */
export function initMusic(run) {
  state.run = run;
  const panel = el("section", "mu", "music");
  panel.setAttribute("aria-label", "Music");
  const h = el("h2");
  h.textContent = "Music";
  const bar = el("div", "mu-bar");
  bar.append(
    button("music-choose", "Choose file…", "Choose an MP3, AAC, WAV or FLAC file", choose),
    button("music-remove", "Remove music", "Remove the music from the edit", remove),
  );
  const status = el("p", "mu-status", "music-status");
  status.hidden = true;
  const spinner = el("span", "mu-spinner");
  const statusText = el("span", "", "music-status-text");
  status.append(spinner, statusText);
  const error = el("p", "mu-error", "music-error");
  error.setAttribute("role", "alert");
  error.hidden = true;

  const current = el("div", "mu-current", "music-current");
  const head = el("div", "mu-head");
  head.append(
    el("span", "mu-title", "music-title"),
    el("span", "mu-duration", "music-duration"),
    el("span", "mu-tempo", "music-tempo"),
  );
  current.append(head, el("div", "mu-strip", "music-beats"));
  const empty = el("p", "placeholder", "music-empty");
  empty.textContent = "No music yet.";

  const bundledHead = el("h3", "mu-subhead", "music-bundled-heading");
  bundledHead.textContent = "Included tracks";
  const bundled = el("ul", "mu-bundled", "music-bundled");
  panel.append(h, bar, status, error, empty, current, bundledHead, bundled);
  document.querySelector("[data-testid=library]").append(panel);

  api.onEvent("kv://job", onJob);
  api
    .invoke("bundled_tracks")
    .then((tracks) => {
      state.bundled = Array.isArray(tracks) ? tracks : [];
      renderBundled();
    })
    .catch(() => {
      state.bundled = [];
      renderBundled();
    });
  renderBundled();
  renderStatus();
}

async function choose() {
  let path;
  try {
    path = await api.invoke("pick_music");
  } catch (e) {
    return fail(e);
  }
  if (typeof path === "string" && path) analyze(path, basename(path));
}

async function analyze(path, name) {
  const sec = section();
  if (!sec) return;
  state.error = null;
  const mine = { id: null, name, fraction: null };
  state.pending = mine;
  renderStatus();
  let job;
  try {
    job = await api.invoke("analyze_music", { section: sec.id, path });
  } catch (e) {
    if (state.pending === mine) state.pending = null;
    return fail(e);
  }
  if (state.pending !== mine) return;
  mine.id = job && typeof job.id === "number" ? job.id : null;
  const early = state.early.get(mine.id);
  state.early.clear();
  if (early) onJob(early);
  else renderStatus();
}

function fail(e) {
  state.error = e && e.message ? e.message : String(e);
  renderStatus();
}

function remove() {
  const sec = section();
  if (!sec || !sec.music) return;
  state.run(api.apply({ type: "set_music", section: sec.id, music: null }));
}

function onJob(u) {
  if (!u || u.kind !== KIND) return;
  const p = state.pending;
  if (!p) return;
  if (p.id === null) {
    // analyze_music has not resolved yet; a cache hit can finish first.
    if (FINAL.has(u.state)) state.early.set(u.id, u);
    return;
  }
  if (u.id !== p.id) return;
  if (u.state === "progress" && typeof u.fraction === "number") p.fraction = u.fraction;
  if (FINAL.has(u.state)) {
    state.pending = null;
    if (u.state === "failed") state.error = u.message || `Couldn't analyse "${p.name}".`;
    if (u.state === "done") fetchBeats(true);
  }
  renderStatus();
}

async function fetchBeats(force) {
  const sec = section();
  const path = sec && sec.music ? sec.music.path : null;
  if (!force && path === state.beatsFor && state.beats) return;
  if (path !== state.beatsFor) state.beats = null;
  state.beatsFor = path;
  if (!path) return renderCurrent();
  renderCurrent();
  try {
    const b = await api.getBeats(sec.id);
    if (b && b.music === state.beatsFor) state.beats = b;
  } catch (e) {
    showToast(e && e.message ? e.message : String(e));
  }
  renderCurrent();
}

function renderStatus() {
  const status = $("music-status");
  if (!status) return;
  const p = state.pending;
  status.hidden = !p;
  if (p) {
    const pct = typeof p.fraction === "number" ? ` ${Math.round(p.fraction * 100)}%` : "";
    $("music-status-text").textContent = `Analysing ${p.name}…${pct}`;
  }
  const error = $("music-error");
  error.hidden = state.error === null;
  error.textContent = state.error || "";
  $("music-choose").disabled = !!p;
  for (const b of document.querySelectorAll("[data-testid=music-track]")) b.disabled = !!p;
}

function renderBundled() {
  const ul = $("music-bundled");
  if (!ul) return;
  ul.replaceChildren();
  $("music-bundled-heading").hidden = state.bundled.length === 0;
  ul.hidden = state.bundled.length === 0;
  for (const t of state.bundled) {
    const li = el("li");
    const b = button("music-track", "", `${t.title} by ${t.artist}`, () => analyze(t.path, t.title));
    b.dataset.trackId = t.id;
    b.className = "mu-track";
    const title = el("span", "mu-track-title");
    title.textContent = t.title;
    const artist = el("span", "mu-track-artist");
    artist.textContent = t.artist;
    b.append(title, artist);
    b.disabled = !!state.pending;
    li.append(b);
    ul.append(li);
  }
}

function renderCurrent() {
  const sec = section();
  const music = sec ? sec.music : null;
  $("music-empty").hidden = !!music;
  $("music-current").hidden = !music;
  $("music-remove").disabled = !music;
  if (!music) return;
  const name = basename(music.path);
  $("music-title").textContent = name;
  $("music-title").title = music.path;
  $("music-duration").textContent = formatDuration(music.duration);
  const b = state.beats;
  $("music-tempo").textContent = b ? `${Math.round(b.tempo_bpm)} BPM` : "– BPM";
  const strip = $("music-beats");
  strip.replaceChildren();
  if (!b) return;
  const down = new Set(b.downbeats);
  for (const t of b.beats) {
    const f = stripFraction(t);
    if (f === null) continue;
    const tick = el("div", down.has(t) ? "mu-beat mu-downbeat" : "mu-beat", "music-beat");
    tick.dataset.time = String(t);
    tick.dataset.downbeat = String(down.has(t));
    tick.style.left = `${f * 100}%`;
    strip.append(tick);
  }
}

export function renderMusic(snapshot) {
  state.snapshot = snapshot;
  renderCurrent();
  fetchBeats(false);
}
