// Camera / SD card import: an "Import from <card>" button in the Library
// while a camera card is mounted, and a dialog that lists its recordings,
// copies the chosen ones into a library folder with a progress bar and
// Cancel, then imports the copies.
//
// Backend (src/camera.rs): invoke("list_cameras") resolves to
// [{ path, name, is_gopro }], asked on window focus and every POLL_MS while
// the page is visible. invoke("list_camera_recordings", { volume }) to
// [{ files, total_bytes, modified (Unix ms or null), chapters, has_gaps,
// already_copied }]. invoke("camera_library_dir") to the library folder;
// invoke("choose_camera_library_dir") opens a folder picker and resolves to
// the chosen (and remembered) folder or null. invoke("start_camera_import",
// { volume, recordings: [first file of each], library_dir }) to the import
// id; its updates arrive as kv://camera { id, state, ... } with state
// "started" ({ total_bytes, files }), "progress" ({ bytes_done,
// total_bytes, file }), "done" ({ copied, skipped, report }), "failed"
// ({ message }) or "cancelled". The imported items arrive as kv://state.
// invoke("cancel_camera_import", { id }).

import * as api from "./api.js";
import { formatEta } from "./export.js";
import { showToast } from "./toast.js";

export const POLL_MS = 3000;
const FINAL = new Set(["done", "failed", "cancelled"]);

/** "7.5 GB" (decimal units, as Finder shows them). */
export function formatBytes(n) {
  const units = ["bytes", "KB", "MB", "GB", "TB"];
  let v = Math.max(0, Number(n) || 0);
  let i = 0;
  while (v >= 1000 && i < units.length - 1) {
    v /= 1000;
    i += 1;
  }
  if (i === 0) return `${v} bytes`;
  return `${v >= 100 ? Math.round(v) : v.toFixed(1)} ${units[i]}`;
}

/** Milliseconds left at the average rate so far; null when unknown. */
export function etaMs(bytesDone, totalBytes, elapsedMs) {
  if (!(bytesDone > 0) || !(elapsedMs > 0)) return null;
  return ((totalBytes - bytesDone) * elapsedMs) / bytesDone;
}

const state = {
  volumes: [],
  polling: false,
  /** The volume the dialog shows. */
  volume: null,
  recordings: [],
  libraryDir: null,
  /** "list" | "copying" */
  view: "list",
  /** Id of the running import; null while it is being started. */
  id: null,
  early: [],
  started: 0,
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

const $ = (testid) => document.querySelector(`[data-testid="${testid}"]`);
const message = (e) => (e && e.message ? e.message : String(e));
const basename = (path) => String(path).split(/[\\/]/).pop();

/** Add the Library's camera buttons and the (hidden) dialog; start polling. */
export function initCamera() {
  const buttons = el("span", "cam-buttons", "camera-buttons");
  document.querySelector("[data-testid=library] .lib-bar").append(buttons);

  const backdrop = el("div", "cam-backdrop", "camera-dialog");
  backdrop.hidden = true;
  // role="dialog" only while shown (other modules treat any dialog as open).
  const dialog = el("div", "cam-dialog", "camera-dialog-box");
  dialog.setAttribute("aria-modal", "true");
  dialog.setAttribute("aria-labelledby", "camera-title");
  const title = el("h2", "", "camera-title");
  title.id = "camera-title";

  const listView = el("div", "cam-list-view", "camera-list-view");
  const pick = el("div", "cam-pick");
  pick.append(
    button("camera-select-all", "Select all", () => selectAll(true)),
    button("camera-select-none", "Select none", () => selectAll(false)),
    el("span", "cam-total", "camera-total"),
  );
  const list = el("ul", "cam-list", "camera-list");
  list.setAttribute("aria-label", "Recordings");
  list.addEventListener("change", renderTotal);
  const dest = el("div", "cam-dest");
  dest.append(
    el("span", "cam-label", "", "Copy to"),
    el("span", "cam-dest-path", "camera-dest"),
    button("camera-change-dest", "Change…", changeDest),
  );
  const status = el("p", "cam-status", "camera-status");
  status.hidden = true;
  const listButtons = el("div", "cam-buttons-row");
  listButtons.append(
    button("camera-close", "Close", closeDialog),
    button("camera-start", "Copy and import", start, "cam-primary"),
  );
  listView.append(pick, list, dest, status, listButtons);

  const copying = el("div", "cam-copying", "camera-copying");
  const bar = el("progress", "cam-bar", "camera-progress");
  bar.max = 1;
  bar.value = 0;
  const line = el("p", "cam-line");
  line.append(el("span", "", "camera-bytes"), el("span", "cam-eta", "camera-eta"));
  const copyButtons = el("div", "cam-buttons-row");
  copyButtons.append(button("camera-cancel", "Cancel", cancel));
  copying.append(el("p", "cam-file", "camera-file"), bar, line, copyButtons);

  dialog.append(title, listView, copying);
  backdrop.append(dialog);
  document.body.append(backdrop);
  document.addEventListener("keydown", (e) => {
    if (e.key === "Escape" && !backdrop.hidden && state.view !== "copying") closeDialog();
  });
  api.onEvent("kv://camera", onUpdate);
  window.addEventListener("focus", poll);
  setInterval(() => {
    if (document.visibilityState === "visible") poll();
  }, POLL_MS);
  poll();
}

/** Ask for the mounted cameras and update the buttons. */
async function poll() {
  if (state.polling) return;
  state.polling = true;
  try {
    const volumes = await api.invoke("list_cameras");
    state.volumes = Array.isArray(volumes) ? volumes : [];
  } catch (e) {
    // Polling: no toast every few seconds; just show no camera.
    console.warn("list_cameras failed:", message(e));
    state.volumes = [];
  } finally {
    state.polling = false;
  }
  renderButtons();
}

function renderButtons() {
  const box = $("camera-buttons");
  const want = state.volumes.map((v) => v.path);
  const have = [...box.children].map((b) => b.dataset.volume);
  if (want.join("\n") === have.join("\n")) return;
  box.replaceChildren(
    ...state.volumes.map((v) => {
      const b = button("camera-import", `Import from ${v.name}`, () => openDialog(v));
      b.dataset.volume = v.path;
      b.title = `Copy recordings from ${v.path} and import them`;
      return b;
    }),
  );
}

async function openDialog(volume) {
  state.volume = volume;
  state.view = "list";
  state.recordings = [];
  $("camera-title").textContent = `Import from ${volume.name}`;
  $("camera-status").hidden = true;
  $("camera-list").replaceChildren();
  $("camera-dialog").hidden = false;
  $("camera-dialog-box").setAttribute("role", "dialog");
  renderView();
  try {
    state.libraryDir = await api.invoke("camera_library_dir");
    $("camera-dest").textContent = state.libraryDir;
    await loadRecordings();
  } catch (e) {
    closeDialog();
    showToast(message(e));
  }
}

/** (Re)list the card's recordings; already-copied ones start unticked. */
async function loadRecordings() {
  const recs = await api.invoke("list_camera_recordings", { volume: state.volume.path });
  state.recordings = Array.isArray(recs) ? recs : [];
  const list = $("camera-list");
  list.replaceChildren(...state.recordings.map(row));
  if (state.recordings.length === 0) {
    list.append(el("li", "cam-empty", "camera-empty", "No videos on this card."));
  }
  renderTotal();
}

function row(rec, i) {
  const li = el("li", "cam-item", "camera-recording");
  li.dataset.copied = String(!!rec.already_copied);
  const label = el("label", "cam-row");
  const box = el("input", "", "camera-check");
  box.type = "checkbox";
  box.checked = !rec.already_copied;
  box.dataset.index = String(i);
  const name = el("span", "cam-name", "camera-name", basename(rec.files[0]));
  const date = el(
    "span",
    "cam-date",
    "camera-date",
    rec.modified == null ? "" : new Date(rec.modified).toLocaleString(),
  );
  const chapters = el(
    "span",
    "cam-chapters",
    "camera-chapters",
    rec.chapters > 1 ? `${rec.chapters} chapters` : "",
  );
  const size = el("span", "cam-size", "camera-size", formatBytes(rec.total_bytes));
  label.append(box, name, date, chapters, size);
  if (rec.already_copied) label.append(el("span", "cam-tag", "camera-copied", "Copied"));
  li.title = rec.files.map(basename).join(", ");
  li.append(label);
  return li;
}

function checked() {
  return [...document.querySelectorAll('[data-testid="camera-check"]')]
    .filter((b) => b.checked)
    .map((b) => state.recordings[Number(b.dataset.index)]);
}

function selectAll(on) {
  for (const b of document.querySelectorAll('[data-testid="camera-check"]')) b.checked = on;
  renderTotal();
}

function renderTotal() {
  const chosen = checked();
  const bytes = chosen.reduce((n, r) => n + r.total_bytes, 0);
  $("camera-total").textContent =
    `${chosen.length} of ${state.recordings.length} selected · ${formatBytes(bytes)}`;
  $("camera-start").disabled = chosen.length === 0 || state.id !== null || state.view !== "list";
}

function renderView() {
  $("camera-list-view").hidden = state.view !== "list";
  $("camera-copying").hidden = state.view !== "copying";
  renderTotal();
}

function closeDialog() {
  if (state.view === "copying") return;
  $("camera-dialog").hidden = true;
  $("camera-dialog-box").removeAttribute("role");
  state.volume = null;
}

async function changeDest() {
  try {
    const dir = await api.invoke("choose_camera_library_dir");
    if (typeof dir !== "string" || !dir) return;
    state.libraryDir = dir;
    $("camera-dest").textContent = dir;
    await loadRecordings();
  } catch (e) {
    showToast(message(e));
  }
}

async function start() {
  const chosen = checked();
  if (chosen.length === 0 || state.view !== "list") return;
  state.view = "copying";
  state.id = null;
  state.early = [];
  state.started = Date.now();
  $("camera-file").textContent = `Copying ${chosen.length} recording${chosen.length === 1 ? "" : "s"}…`;
  const total = chosen.reduce((n, r) => n + r.total_bytes, 0);
  setProgress(0, total);
  renderView();
  try {
    const id = await api.invoke("start_camera_import", {
      volume: state.volume.path,
      recordings: chosen.map((r) => r.files[0]),
      library_dir: state.libraryDir,
    });
    state.id = id;
    const early = state.early.filter((u) => u.id === id);
    state.early = [];
    for (const u of early) onUpdate(u);
  } catch (e) {
    state.view = "list";
    renderView();
    showToast(message(e));
  }
}

function setProgress(done, total) {
  $("camera-progress").value = total > 0 ? Math.min(1, done / total) : 0;
  $("camera-bytes").textContent = `${formatBytes(done)} of ${formatBytes(total)}`;
  const eta = etaMs(done, total, Date.now() - state.started);
  $("camera-eta").textContent = eta === null ? "" : formatEta(eta);
}

function onUpdate(u) {
  if (!u || typeof u.id !== "number") return;
  if (state.view === "copying" && state.id === null) {
    state.early.push(u);
    return;
  }
  if (u.id !== state.id || state.view !== "copying") return;
  if (u.state === "started") state.started = Date.now();
  if (u.state === "progress") setProgress(u.bytes_done, u.total_bytes);
  if (!FINAL.has(u.state)) return;
  state.id = null;
  state.view = "list";
  if (u.state === "done") {
    const name = state.volume ? state.volume.name : "the card";
    closeDialog();
    const added = u.report && u.report.added ? u.report.added.length : 0;
    showToast(`Imported ${added} item${added === 1 ? "" : "s"} from ${name}.`, { kind: "info" });
    return;
  }
  if (u.state === "failed") {
    showToast(u.message || "The copy failed.");
  } else {
    $("camera-status").textContent = "Copy cancelled. Files already copied were kept.";
    $("camera-status").hidden = false;
  }
  renderView();
  if (state.volume) loadRecordings().catch((e) => showToast(message(e)));
}

async function cancel() {
  if (state.id === null) return;
  try {
    await api.invoke("cancel_camera_import", { id: state.id });
  } catch (e) {
    showToast(message(e));
  }
}
