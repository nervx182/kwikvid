// Project files: the File menu flows (New, Open, Open Recent, Save, Save As,
// Quit), the window title with its edited marker, the unsaved-changes
// prompt and the missing-media banner.
//
// The native menu (src/menu.rs) emits kv://menu with { action, path? }; the
// backend also sends { action: "quit" } when the window is closed with
// unsaved changes. Every project command returns
// { snapshot, file, missing } (src/project.rs).

import * as api from "./api.js";
import { showToast } from "./toast.js";

const MENU_EVENT = "kv://menu";
const APP_NAME = "kwikvid";
/** The edited marker in the window title. */
const EDITED = "• ";

let render = () => {};
let file = null;
let dirty = false;
let busy = false;
let lastTitle = null;

/** "trip" for "/Users/ana/Movies/trip.kwikvid" (any OS separator). */
export function projectName(path) {
  if (!path) return null;
  const base = String(path).split(/[\\/]/).pop();
  return base.replace(/\.kwikvid$/i, "") || base;
}

/** The window title: "• trip — kwikvid"; untitled projects show "kwikvid". */
export function windowTitle(path, isDirty) {
  const name = projectName(path);
  const base = name ? `${name} — ${APP_NAME}` : APP_NAME;
  return isDirty ? EDITED + base : base;
}

function updateTitle() {
  const title = windowTitle(file, dirty);
  if (title === lastTitle) return;
  lastTitle = title;
  document.title = title;
  // The native title; a failure only leaves the old title, so stay quiet.
  api.invoke("set_window_title", { title }).catch(() => {});
}

/** Called for every rendered snapshot (main.js). */
export function renderProject(snapshot) {
  dirty = !!snapshot.dirty;
  updateTitle();
}

/** Take a project command's result: file, snapshot and missing media. */
function accept(result) {
  if (!result) return;
  if ("file" in result) file = result.file ?? null;
  if (result.snapshot) render(result.snapshot);
  updateTitle();
}

// ---- Unsaved-changes prompt -------------------------------------------------

/** Ask Save / Don't Save / Cancel; resolves to "save", "discard" or "cancel". */
export function askToSave() {
  return new Promise((resolve) => {
    const back = document.createElement("div");
    back.className = "project-modal-backdrop";
    const box = document.createElement("div");
    box.className = "project-modal";
    box.dataset.testid = "unsaved-prompt";
    box.setAttribute("role", "alertdialog");
    box.setAttribute("aria-modal", "true");
    box.setAttribute("aria-labelledby", "unsaved-prompt-title");
    const name = projectName(file) ?? "Untitled";
    const title = document.createElement("h3");
    title.id = "unsaved-prompt-title";
    title.textContent = `Do you want to save the changes to “${name}”?`;
    const note = document.createElement("p");
    note.textContent = "Your changes will be lost if you don't save them.";
    const buttons = document.createElement("div");
    buttons.className = "project-modal-buttons";
    const done = (answer) => {
      document.removeEventListener("keydown", onKey, true);
      back.remove();
      resolve(answer);
    };
    const onKey = (e) => {
      if (e.key === "Escape") {
        e.preventDefault();
        e.stopPropagation();
        done("cancel");
      }
    };
    for (const [answer, label] of [
      ["discard", "Don't Save"],
      ["cancel", "Cancel"],
      ["save", "Save"],
    ]) {
      const b = document.createElement("button");
      b.type = "button";
      b.textContent = label;
      b.dataset.testid = `prompt-${answer}`;
      if (answer === "save") b.className = "primary";
      b.addEventListener("click", () => done(answer));
      buttons.append(b);
    }
    box.append(title, note, buttons);
    back.append(box);
    document.body.append(back);
    document.addEventListener("keydown", onKey, true);
    box.querySelector(".primary").focus();
  });
}

/** True when the current project may be replaced or closed. */
async function confirmDiscard() {
  if (!dirty) return true;
  const answer = await askToSave();
  if (answer === "cancel") return false;
  if (answer === "discard") return true;
  return save();
}

// ---- Missing-media banner ---------------------------------------------------

function plural(n, one, many) {
  return `${n} ${n === 1 ? one : many}`;
}

/** "3 clips not found", "Music file not found", "2 clips and 1 music file not found". */
export function missingSummary(missing) {
  const clips = missing.filter((m) => m.kind && m.kind.type === "media").length;
  const music = missing.length - clips;
  if (clips && music) {
    return `${plural(clips, "clip", "clips")} and ${plural(music, "music file", "music files")} not found`;
  }
  if (clips) return `${plural(clips, "clip", "clips")} not found`;
  return music === 1 ? "Music file not found" : `${music} music files not found`;
}

function hideBanner() {
  document.querySelector('[data-testid="missing-media"]')?.remove();
}

function showBanner(missing) {
  hideBanner();
  if (!missing || missing.length === 0) return;
  const el = document.createElement("div");
  el.className = "missing-media";
  el.dataset.testid = "missing-media";
  el.setAttribute("role", "alert");
  const head = document.createElement("div");
  head.className = "missing-media-head";
  const text = document.createElement("strong");
  text.dataset.testid = "missing-media-summary";
  text.textContent = missingSummary(missing);
  const close = document.createElement("button");
  close.type = "button";
  close.textContent = "Dismiss";
  close.dataset.testid = "missing-media-dismiss";
  close.addEventListener("click", hideBanner);
  head.append(text, close);
  const list = document.createElement("ul");
  for (const m of missing) {
    const li = document.createElement("li");
    li.textContent = m.resolved ?? m.project_path;
    li.title = m.project_path;
    list.append(li);
  }
  el.append(head, list);
  document.body.append(el);
}

// ---- Flows ------------------------------------------------------------------

/** Run a backend call; toast its error. Resolves to the result or null. */
async function call(cmd, args) {
  try {
    return await api.invoke(cmd, args);
  } catch (e) {
    showToast(e && e.message ? e.message : String(e));
    return null;
  }
}

/** Save As: pick a path and save there. True when saved. */
async function saveAs() {
  const path = await call("choose_save_path");
  if (!path) return false;
  const r = await call("save_project_as", { path });
  accept(r);
  return r !== null;
}

/** Save; acts as Save As while untitled. True when saved. */
async function save() {
  if (!file) return saveAs();
  const r = await call("save_project");
  accept(r);
  return r !== null;
}

async function newProject() {
  if (!(await confirmDiscard())) return;
  const r = await call("new_project");
  if (r) hideBanner();
  accept(r);
}

async function open(path) {
  if (!(await confirmDiscard())) return;
  const chosen = path ?? (await call("choose_open_path"));
  if (!chosen) return;
  const r = await call("open_project", { path: chosen });
  if (!r) return;
  accept(r);
  showBanner(r.missing);
}

async function quit() {
  if (!(await confirmDiscard())) return;
  await call("quit");
}

const ACTIONS = {
  new: () => newProject(),
  open: () => open(),
  open_recent: (a) => open(a.path),
  save: () => save(),
  save_as: () => saveAs(),
  quit: () => quit(),
};

/** Run one menu action; ignored while another flow is running. */
async function onMenu(action) {
  const run = action && ACTIONS[action.action];
  if (!run || busy) return;
  busy = true;
  try {
    await run(action);
  } finally {
    busy = false;
  }
}

/** Wire the menu. renderSnapshot(snapshot) renders the shell (main.js). */
export function initProject(renderSnapshot) {
  render = renderSnapshot;
  api.onEvent(MENU_EVENT, onMenu);
  call("project_info").then(accept);
}
