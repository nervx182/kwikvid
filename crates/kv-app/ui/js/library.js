// Media library: import (buttons, or files dropped on the window), one row
// per clip with its name, duration, 12-cell filmstrip and HiLight ticks, a
// spinner while its jobs run, error rows for skipped files, selection and
// remove (context menu or Delete/Backspace).
//
// Backend (src/library.rs): invoke("import", { paths }) and
// invoke("pick_media", { folders }) resolve to an ImportReport
// { added: [id], skipped: [[path, reason]] } (pick_media: null when
// cancelled); the new Snapshot arrives as kv://state. Filmstrip jobs report
// on kv://job ({ id, kind, media_id, state, fraction, message }); the
// thumbnails are kvthumb://<media_id>/<index> images, 404 until the job is
// "done". Removing sends Command::RemoveMedia through apply.
//
// Other components follow the selection through the "kv:media-selected"
// document event ({ detail: { mediaId } }, null when nothing is selected)
// or selectedMedia().

import * as api from "./api.js";
import { showToast } from "./toast.js";
import { FILMSTRIP_CELLS, thumbUrl } from "./timeline.js";

export const SELECT_EVENT = "kv:media-selected";

const state = {
  run: null,
  selected: null,
  /** media id -> Set of unfinished filmstrip job ids. */
  jobs: new Map(),
  /** media id -> failure message of its last filmstrip job. */
  failed: new Map(),
  /** [path, reason] pairs from the last import. */
  skipped: [],
  importing: 0,
  /** media id -> <li>, kept across renders so images are not refetched. */
  rows: new Map(),
};

const FINAL = new Set(["done", "failed", "cancelled"]);

/** "1:15" for 75 s (rounded to the second). */
export function formatDuration(us) {
  const total = Math.max(0, Math.round(us / 1_000_000));
  return `${Math.floor(total / 60)}:${String(total % 60).padStart(2, "0")}`;
}

/** Left offset of a HiLight tick, as a fraction of the filmstrip width. */
export function hilightFraction(hilight, duration) {
  if (!(duration > 0)) return 0;
  return Math.min(1, Math.max(0, hilight / duration));
}

export function selectedMedia() {
  return state.selected;
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

const pane = () => document.querySelector("[data-testid=library]");
const list = () => document.getElementById("library-list");

function button(testid, text, title, onClick) {
  const b = el("button", "", testid);
  b.type = "button";
  b.textContent = text;
  b.title = title;
  b.addEventListener("click", onClick);
  return b;
}

/** Build the pane's controls and wire events. run(promise) renders a snapshot or toasts. */
export function initLibrary(run) {
  state.run = run;
  const root = pane();
  const bar = el("div", "lib-bar");
  const files = button("library-import-files", "Import…", "Import video files", () =>
    importWith(api.invoke("pick_media", { folders: false })),
  );
  const folder = button("library-import-folder", "Folder…", "Import every video in a folder", () =>
    importWith(api.invoke("pick_media", { folders: true })),
  );
  const busy = el("span", "lib-spinner lib-importing", "library-importing");
  busy.id = "library-importing";
  busy.title = "Importing…";
  busy.hidden = true;
  bar.append(files, folder, busy);
  const ul = el("ul", "lib-list", "library-list");
  ul.id = "library-list";
  ul.setAttribute("role", "listbox");
  ul.setAttribute("aria-label", "Clips");
  root.querySelector("h2").after(bar);
  root.append(ul);

  ul.addEventListener("click", (e) => {
    const row = e.target.closest(".lib-item[data-media-id]");
    select(row ? Number(row.dataset.mediaId) : null);
  });
  ul.addEventListener("contextmenu", (e) => {
    const row = e.target.closest(".lib-item[data-media-id]");
    if (!row) return;
    e.preventDefault();
    const id = Number(row.dataset.mediaId);
    select(id);
    openMenu(id, e.clientX, e.clientY);
  });
  document.addEventListener("click", (e) => {
    if (!e.target.closest(".lib-menu")) closeMenu();
  });
  document.addEventListener("keydown", (e) => {
    if (e.key === "Escape") closeMenu();
    if (e.key !== "Delete" && e.key !== "Backspace") return;
    if (state.selected === null || !pane().contains(document.activeElement)) return;
    if (e.metaKey || e.ctrlKey || e.altKey) return;
    e.preventDefault();
    remove(state.selected);
  });

  api.onEvent("kv://job", onJob);
  api.onEvent("tauri://drag-enter", () => root.classList.add("lib-drop"));
  api.onEvent("tauri://drag-leave", () => root.classList.remove("lib-drop"));
  api.onEvent("tauri://drag-drop", (p) => {
    root.classList.remove("lib-drop");
    const paths = (p && p.paths) || [];
    if (paths.length > 0) importWith(api.invoke("import", { paths }));
  });
}

async function importWith(promise) {
  state.importing += 1;
  renderBusy();
  try {
    const report = await promise;
    if (report) {
      state.skipped = report.skipped || [];
      renderSkipped();
    }
  } catch (e) {
    showToast(e && e.message ? e.message : String(e));
  } finally {
    state.importing -= 1;
    renderBusy();
  }
}

function remove(mediaId) {
  closeMenu();
  state.run(api.apply({ type: "remove_media", media_id: mediaId }));
}

function select(mediaId) {
  if (state.selected === mediaId) return;
  state.selected = mediaId;
  for (const [id, row] of state.rows) row.dataset.selected = String(id === mediaId);
  document.dispatchEvent(new CustomEvent(SELECT_EVENT, { detail: { mediaId } }));
}

function openMenu(mediaId, x, y) {
  closeMenu();
  const menu = el("div", "lib-menu", "library-menu");
  menu.setAttribute("role", "menu");
  const item = button("library-remove", "Remove from Library", "", () => remove(mediaId));
  item.removeAttribute("title");
  item.setAttribute("role", "menuitem");
  menu.append(item);
  menu.style.left = `${x}px`;
  menu.style.top = `${y}px`;
  document.body.append(menu);
}

function closeMenu() {
  for (const m of document.querySelectorAll(".lib-menu")) m.remove();
}

function onJob(u) {
  if (!u || u.kind !== "filmstrip" || u.media_id == null) return;
  const id = u.media_id;
  let jobs = state.jobs.get(id);
  if (!jobs) state.jobs.set(id, (jobs = new Set()));
  if (FINAL.has(u.state)) {
    jobs.delete(u.id);
    if (jobs.size === 0) state.jobs.delete(id);
  } else {
    jobs.add(u.id);
  }
  if (u.state === "done") {
    state.failed.delete(id);
    loadThumbs(id);
  } else if (u.state === "failed") {
    state.failed.set(id, u.message || "Couldn't make thumbnails.");
  }
  const row = state.rows.get(id);
  if (row) renderRowStatus(row, id);
}

/** (Re)load every filmstrip image of an item. */
function loadThumbs(mediaId) {
  const row = state.rows.get(mediaId);
  if (!row) return;
  for (const cell of row.querySelectorAll(".lib-cell")) {
    cell.replaceChildren();
    const img = document.createElement("img");
    img.alt = "";
    img.draggable = false;
    // 404 until the filmstrip job is done: keep the empty cell.
    img.addEventListener("error", () => img.remove());
    img.src = cell.dataset.thumb;
    cell.append(img);
  }
}

function makeRow(item) {
  const row = el("li", "lib-item", "library-item");
  row.dataset.mediaId = String(item.id);
  row.dataset.selected = String(item.id === state.selected);
  row.tabIndex = 0;
  row.setAttribute("role", "option");
  const head = el("div", "lib-head");
  const name = el("span", "lib-name", "library-name");
  const spinner = el("span", "lib-spinner", "library-spinner");
  spinner.title = "Making thumbnails…";
  const badge = el("span", "lib-badge", "library-error");
  badge.textContent = "!";
  const dur = el("span", "lib-duration", "library-duration");
  head.append(name, spinner, badge, dur);
  const strip = el("div", "lib-strip", "library-filmstrip");
  for (let i = 0; i < FILMSTRIP_CELLS; i++) {
    const cell = el("div", "lib-cell");
    cell.dataset.thumb = thumbUrl(item.id, i);
    strip.append(cell);
  }
  row.append(head, strip);
  state.rows.set(item.id, row);
  loadThumbs(item.id);
  return row;
}

function updateRow(row, item) {
  const name = basename(item.path);
  row.querySelector(".lib-name").textContent = name;
  const chapters = item.chapters ? item.chapters.length : 0;
  row.title = chapters > 1 ? `${name} (${chapters} chapters)` : name;
  row.querySelector(".lib-duration").textContent = formatDuration(item.duration);
  const strip = row.querySelector(".lib-strip");
  for (const t of strip.querySelectorAll(".lib-hilight")) t.remove();
  for (const h of item.hilights) {
    const tick = el("div", "lib-hilight", "library-hilight");
    tick.dataset.time = String(h);
    tick.style.left = `${hilightFraction(h, item.duration) * 100}%`;
    tick.title = `HiLight at ${formatDuration(h)}`;
    strip.append(tick);
  }
  renderRowStatus(row, item.id);
}

function renderRowStatus(row, id) {
  const busy = state.jobs.has(id);
  row.dataset.busy = String(busy);
  row.querySelector(".lib-spinner").hidden = !busy;
  const badge = row.querySelector(".lib-badge");
  const failure = state.failed.get(id);
  badge.hidden = failure === undefined;
  badge.title = failure || "";
}

function renderBusy() {
  const b = document.getElementById("library-importing");
  if (b) b.hidden = state.importing === 0;
}

function renderSkipped() {
  const ul = list();
  for (const r of ul.querySelectorAll(".lib-skipped")) r.remove();
  state.skipped.forEach(([path, reason], i) => {
    const row = el("li", "lib-item lib-skipped", "library-skipped");
    row.title = `${path}\n${reason}`;
    const head = el("div", "lib-head");
    const name = el("span", "lib-name", "library-name");
    name.textContent = basename(path);
    const badge = el("span", "lib-badge", "library-error");
    badge.textContent = "!";
    badge.title = reason;
    const close = button("library-dismiss", "×", "Dismiss", (e) => {
      e.stopPropagation();
      state.skipped.splice(i, 1);
      renderSkipped();
    });
    close.className = "lib-dismiss";
    head.append(name, badge, close);
    row.append(head);
    ul.append(row);
  });
}

export function renderLibrary(snapshot) {
  const media = snapshot.project.media;
  const ids = new Set(media.map((m) => m.id));
  for (const [id, row] of state.rows) {
    if (!ids.has(id)) {
      row.remove();
      state.rows.delete(id);
    }
  }
  const ul = list();
  const firstSkipped = ul.querySelector(".lib-skipped");
  for (const item of media) {
    const row = state.rows.get(item.id) || makeRow(item);
    updateRow(row, item);
    ul.insertBefore(row, firstSkipped); // project order, clips before skipped rows
  }
  if (state.selected !== null && !ids.has(state.selected)) select(null);
}
