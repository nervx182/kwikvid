// Export dialog: codec, resolution and aspect ratio, a save dialog, then a
// progress bar with ETA and Cancel, and on success the report with "Show in
// Finder".
//
// Backend (src/export.rs): invoke("export_options", { section }) resolves to
// { codecs, aspect, resolution, file_name, blocked } (codecs: "H264" and,
// where it can be encoded, "hevc"; blocked: why exporting is not possible,
// or null). invoke("choose_export_path", { name }) to an absolute path
// or null. invoke("start_export", { section, request: { codec, resolution,
// aspect, path } }) to the export id; its updates arrive as kv://export
// { id, state, ... } with state "started", "progress" ({ frame,
// total_frames, fraction, eta_ms }), "done" ({ report: { path, frames,
// duration, encoder_name, elapsed_ms } }), "failed" ({ message }) or
// "cancelled". invoke("cancel_export", { id }); invoke("reveal_export",
// { path }). Changing the aspect ratio sends Command::SetAspectRatio through
// apply, so the preview letterbox follows.

import * as api from "./api.js";
import { formatDuration } from "./library.js";
import { showToast } from "./toast.js";

export const ASPECTS = [
  ["landscape16x9", "16:9 landscape"],
  ["portrait9x16", "9:16 portrait"],
  ["square1x1", "1:1 square"],
];
export const RESOLUTIONS = [
  ["1080p", "1080p"],
  ["4k", "4K"],
];
const CODEC_LABELS = { H264: "H.264", hevc: "HEVC (H.265)" };
const FINAL = new Set(["done", "failed", "cancelled"]);
const FPS = 30;

/** Output size, as kv_session::export_size: [width, height]. */
export function exportSize(aspect, resolution) {
  const [long, short] = resolution === "4k" ? [3840, 2160] : [1920, 1080];
  if (aspect === "portrait9x16") return [short, long];
  if (aspect === "square1x1") return [short, short];
  return [long, short];
}

/** "About 1:05 left" from milliseconds; "" when unknown. */
export function formatEta(ms) {
  if (typeof ms !== "number" || !(ms >= 0)) return "";
  const s = Math.ceil(ms / 1000);
  if (s < 1) return "Almost done";
  return `About ${Math.floor(s / 60)}:${String(s % 60).padStart(2, "0")} left`;
}

/** "12.3 s" from milliseconds. */
export function formatElapsed(ms) {
  return `${(ms / 1000).toFixed(1)} s`;
}

/** Why the edit cannot be exported, from the snapshot alone (null: maybe it can). */
export function snapshotBlocker(snapshot) {
  const sec = snapshot && snapshot.project.sections[0];
  if (!sec || sec.edit.segments.length === 0) {
    return "Nothing to export yet: add clips and music to make an edit.";
  }
  return null;
}

const state = {
  run: null,
  snapshot: null,
  options: null,
  /** "form" | "running" | "done" | "failed" */
  view: "form",
  /** Id of the running export; null while it is being started. */
  id: null,
  starting: false,
  /** Updates that arrived before start_export resolved. */
  early: [],
  report: null,
  /** The project's aspect ratio in the last snapshot. */
  projectAspect: null,
};

function el(tag, className, testid, text) {
  const e = document.createElement(tag);
  if (className) e.className = className;
  if (testid) e.dataset.testid = testid;
  if (text !== undefined) e.textContent = text;
  return e;
}

function button(testid, text, onClick, className = "") {
  const b = el("button", className, testid, text);
  b.type = "button";
  b.addEventListener("click", onClick);
  return b;
}

function select(testid, label) {
  const wrap = el("label", "ex-field");
  wrap.append(el("span", "ex-label", "", label));
  const s = el("select", "", testid);
  wrap.append(s);
  return [wrap, s];
}

function fill(sel, entries) {
  sel.replaceChildren(
    ...entries.map(([value, text]) => {
      const o = document.createElement("option");
      o.value = value;
      o.textContent = text;
      return o;
    }),
  );
}

const $ = (testid) => document.querySelector(`[data-testid="${testid}"]`);

function sectionId() {
  const s = state.snapshot;
  return s && s.project.sections.length > 0 ? s.project.sections[0].id : 1;
}

/** Add the toolbar button and the (hidden) dialog; listen for kv://export. */
export function initExport(run) {
  state.run = run;
  const open = button("export-open", "Export…", openDialog, "ex-open");
  open.disabled = true;
  document.querySelector("[data-testid=toolbar]").append(open);

  const backdrop = el("div", "ex-backdrop", "export-dialog");
  backdrop.hidden = true;
  // role="dialog" only while shown: other modules (the transport keys)
  // treat any dialog in the page as open.
  const dialog = el("div", "ex-dialog", "export-dialog-box");
  dialog.setAttribute("aria-modal", "true");
  dialog.setAttribute("aria-label", "Export");
  dialog.append(el("h2", "", "", "Export"));

  // Options.
  const form = el("div", "ex-form", "export-form");
  const [codecField, codec] = select("export-codec", "Codec");
  const [resField, res] = select("export-resolution", "Resolution");
  fill(res, RESOLUTIONS);
  const [aspectField, aspect] = select("export-aspect", "Aspect ratio");
  fill(aspect, ASPECTS);
  res.addEventListener("change", renderSize);
  aspect.addEventListener("change", () => {
    renderSize();
    if (state.snapshot && state.snapshot.project.settings.aspect_ratio !== aspect.value) {
      state.run(api.apply({ type: "set_aspect_ratio", aspect: aspect.value }));
    }
  });
  const size = el("p", "ex-size", "export-size");
  const blocked = el("p", "ex-blocked", "export-blocked");
  blocked.setAttribute("role", "alert");
  blocked.hidden = true;
  const status = el("p", "ex-status", "export-status");
  status.hidden = true;
  const formButtons = el("div", "ex-buttons");
  formButtons.append(
    button("export-close", "Close", closeDialog),
    button("export-start", "Export…", start, "ex-primary"),
  );
  form.append(codecField, resField, aspectField, size, blocked, status, formButtons);

  // Progress.
  const running = el("div", "ex-running", "export-running");
  const bar = el("progress", "ex-bar", "export-progress");
  bar.max = 1;
  bar.value = 0;
  const line = el("p", "ex-line");
  line.append(el("span", "", "export-percent", "0%"), el("span", "ex-eta", "export-eta"));
  const runButtons = el("div", "ex-buttons");
  runButtons.append(button("export-cancel", "Cancel", cancel));
  running.append(el("p", "ex-file", "export-file"), bar, line, runButtons);

  // Done.
  const done = el("div", "ex-done", "export-done");
  const report = el("dl", "ex-report", "export-report");
  const doneButtons = el("div", "ex-buttons");
  doneButtons.append(
    button("export-done-close", "Close", closeDialog),
    button("export-reveal", "Show in Finder", reveal, "ex-primary"),
  );
  done.append(el("p", "ex-ok", "", "Export finished."), report, doneButtons);

  // Failed.
  const failed = el("div", "ex-failed", "export-failed");
  const error = el("p", "ex-error", "export-error");
  error.setAttribute("role", "alert");
  const failButtons = el("div", "ex-buttons");
  failButtons.append(button("export-failed-close", "Close", closeDialog));
  failed.append(error, failButtons);

  dialog.append(form, running, done, failed);
  backdrop.append(dialog);
  document.body.append(backdrop);
  document.addEventListener("keydown", (e) => {
    if (e.key === "Escape" && !backdrop.hidden && state.view !== "running") closeDialog();
  });
  api.onEvent("kv://export", onUpdate);
  renderView();
}

/** Keep the toolbar button and the dialog's aspect in step with the state. */
export function renderExport(snapshot) {
  state.snapshot = snapshot;
  const open = $("export-open");
  const why = snapshotBlocker(snapshot);
  open.disabled = why !== null;
  open.title = why || "Export the edit to a video file";
  // Follow the project's aspect when it changes (e.g. undo); a pick the
  // backend has not answered yet is kept.
  const projectAspect = snapshot.project.settings.aspect_ratio;
  if (projectAspect !== state.projectAspect) {
    state.projectAspect = projectAspect;
    if (!$("export-dialog").hidden && state.view === "form") {
      $("export-aspect").value = projectAspect;
      renderSize();
    }
  }
}

async function openDialog() {
  state.view = "form";
  state.options = null;
  $("export-status").hidden = true;
  $("export-dialog").hidden = false;
  $("export-dialog-box").setAttribute("role", "dialog");
  renderView();
  let options;
  try {
    options = await api.invoke("export_options", { section: sectionId() });
  } catch (e) {
    closeDialog();
    showToast(e && e.message ? e.message : String(e));
    return;
  }
  state.options = options;
  const codecs = options.codecs.length > 0 ? options.codecs : ["H264"];
  fill(
    $("export-codec"),
    codecs.map((c) => [c, CODEC_LABELS[c] || c]),
  );
  $("export-resolution").value = options.resolution || "1080p";
  $("export-aspect").value = options.aspect;
  renderSize();
  renderView();
  $("export-codec").focus();
}

function closeDialog() {
  if (state.view === "running") return;
  $("export-dialog").hidden = true;
  $("export-dialog-box").removeAttribute("role");
  state.view = "form";
  state.report = null;
}

function renderSize() {
  const [w, h] = exportSize($("export-aspect").value, $("export-resolution").value);
  $("export-size").textContent = `${w} × ${h} · ${FPS} fps`;
}

function renderView() {
  const v = state.view;
  $("export-form").hidden = v !== "form";
  $("export-running").hidden = v !== "running";
  $("export-done").hidden = v !== "done";
  $("export-failed").hidden = v !== "failed";
  const blocked = state.options ? state.options.blocked : null;
  $("export-blocked").hidden = !blocked;
  $("export-blocked").textContent = blocked || "";
  $("export-start").disabled = !state.options || !!blocked || state.starting;
  for (const id of ["export-codec", "export-resolution", "export-aspect"]) {
    $(id).disabled = !state.options || state.starting;
  }
}

async function start() {
  if (!state.options || state.options.blocked || state.starting) return;
  const request = {
    codec: $("export-codec").value,
    resolution: $("export-resolution").value,
    aspect: $("export-aspect").value,
  };
  state.starting = true;
  renderView();
  try {
    const path = await api.invoke("choose_export_path", { name: state.options.file_name });
    if (typeof path !== "string" || !path) return;
    request.path = path;
    state.early = [];
    state.id = null;
    state.view = "running";
    $("export-file").textContent = `Exporting ${basename(path)}…`;
    setProgress(0, null);
    renderView();
    const id = await api.invoke("start_export", { section: sectionId(), request });
    state.id = id;
    const early = state.early.filter((u) => u.id === id);
    state.early = [];
    for (const u of early) onUpdate(u);
  } catch (e) {
    state.view = "form";
    showToast(e && e.message ? e.message : String(e));
  } finally {
    state.starting = false;
    renderView();
  }
}

function basename(path) {
  return String(path).split(/[\\/]/).pop();
}

function setProgress(fraction, etaMs) {
  const f = Math.max(0, Math.min(1, fraction || 0));
  $("export-progress").value = f;
  $("export-percent").textContent = `${Math.floor(f * 100)}%`;
  $("export-eta").textContent = formatEta(etaMs);
}

function onUpdate(u) {
  if (!u || typeof u.id !== "number") return;
  if (state.view === "running" && state.id === null) {
    state.early.push(u);
    return;
  }
  if (u.id !== state.id || state.view !== "running") return;
  if (u.state === "progress") setProgress(u.fraction, u.eta_ms);
  if (!FINAL.has(u.state)) return;
  state.id = null;
  if (u.state === "done") {
    state.report = u.report;
    state.view = "done";
    renderReport(u.report);
  } else if (u.state === "failed") {
    state.view = "failed";
    $("export-error").textContent = u.message || "The export failed.";
  } else {
    state.view = "form";
    $("export-status").textContent = "Export cancelled. No file was written.";
    $("export-status").hidden = false;
  }
  renderView();
}

function renderReport(r) {
  const dl = $("export-report");
  dl.replaceChildren();
  for (const [key, k, v] of [
    ["file", "File", basename(r.path)],
    ["duration", "Duration", formatDuration(r.duration)],
    ["encoder", "Encoder", r.encoder_name],
    ["elapsed", "Took", formatElapsed(r.elapsed_ms)],
  ]) {
    const dd = el("dd", "", `export-report-${key}`, v);
    dl.append(el("dt", "", "", k), dd);
  }
}

async function cancel() {
  if (state.id === null) return;
  try {
    await api.invoke("cancel_export", { id: state.id });
  } catch (e) {
    showToast(e && e.message ? e.message : String(e));
  }
}

async function reveal() {
  if (!state.report) return;
  try {
    await api.invoke("reveal_export", { path: state.report.path });
  } catch (e) {
    showToast(e && e.message ? e.message : String(e));
  }
}
