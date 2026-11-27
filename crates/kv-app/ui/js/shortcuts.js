// Keyboard shortcuts overview: Help > Keyboard Shortcuts (kv://menu
// { action: "keyboard_shortcuts" }, CmdOrCtrl+/) or ? opens an overlay that
// lists every shortcut, grouped.
//
// One source: each feature module exports the keys its own handler acts on
// (`KEYS`), the native menu's accelerators are MENU_KEYS below (checked
// against src/menu.rs SHORTCUTS by a Rust test), and the overlay is built
// from SHORTCUTS, which collects them all.
//
// Entry: { group, keys, description }. `keys` are alternatives in Tauri
// accelerator notation ("CmdOrCtrl+Shift+Z", "Shift+Left", "?"); CmdOrCtrl
// shows as Cmd on macOS and Ctrl elsewhere.
//
// While the overlay is open it takes every key (a window capture listener),
// so no other handler acts: Escape closes it and Tab stays inside it.
// Opening it sends no command and leaves playback alone.

import * as api from "./api.js";
import { KEYS as TOOLBAR_KEYS } from "./toolbar.js";
import { KEYS as TRANSPORT_KEYS } from "./transport.js";
import { KEYS as TWEAKS_KEYS } from "./tweaks.js";
import { KEYS as INSPECTOR_KEYS } from "./inspector.js";

const MENU_EVENT = "kv://menu";
const MENU_ACTION = "keyboard_shortcuts";

/** Groups in display order. */
export const GROUPS = ["File", "Edit", "Playback", "Timeline", "Inspector", "View"];

/** Native menu accelerators (src/menu.rs SHORTCUTS) and the overview itself. */
export const MENU_KEYS = [
  { group: "File", keys: ["CmdOrCtrl+N"], description: "New project" },
  { group: "File", keys: ["CmdOrCtrl+O"], description: "Open project" },
  { group: "File", keys: ["CmdOrCtrl+S"], description: "Save" },
  { group: "File", keys: ["CmdOrCtrl+Shift+S"], description: "Save As" },
  { group: "File", keys: ["CmdOrCtrl+Q"], description: "Quit" },
  { group: "View", keys: ["?", "CmdOrCtrl+/"], description: "Keyboard shortcuts" },
];

/** Every shortcut, in group order. */
export const SHORTCUTS = [
  ...MENU_KEYS,
  ...TOOLBAR_KEYS,
  ...TRANSPORT_KEYS,
  ...TWEAKS_KEYS,
  ...INSPECTOR_KEYS,
].sort((a, b) => GROUPS.indexOf(a.group) - GROUPS.indexOf(b.group));

const KEY_NAMES = {
  Left: "←",
  Right: "→",
  Up: "↑",
  Down: "↓",
  Escape: "Esc",
};

export function isMac() {
  const p = (navigator.userAgentData && navigator.userAgentData.platform) || navigator.platform || "";
  return /mac|iphone|ipad/i.test(p) || /Macintosh/.test(navigator.userAgent);
}

/** The parts of one accelerator as shown, e.g. ["Ctrl", "Shift", "Z"]. */
export function keyParts(accelerator, mac = isMac()) {
  // "CmdOrCtrl+/" splits fine; no shortcut uses "+" as its key.
  return accelerator.split("+").map((k) => {
    if (k === "CmdOrCtrl") return mac ? "Cmd" : "Ctrl";
    return KEY_NAMES[k] || k;
  });
}

/** "Ctrl+Shift+Z" (or "Cmd+Shift+Z" on macOS). */
export function formatKeys(accelerator, mac = isMac()) {
  return keyParts(accelerator, mac).join("+");
}

// ---- Overlay ---------------------------------------------------------------

const state = { backdrop: null, returnFocus: null };

function el(tag, className, testid, text) {
  const e = document.createElement(tag);
  if (className) e.className = className;
  if (testid) e.dataset.testid = testid;
  if (text !== undefined) e.textContent = text;
  return e;
}

function typing(t) {
  return t && (t.isContentEditable || ["INPUT", "TEXTAREA", "SELECT"].includes(t.tagName));
}

/** Another dialog (export, unsaved-changes prompt) is showing. */
function otherDialogOpen() {
  return [...document.querySelectorAll('[role="alertdialog"], [role="dialog"], dialog[open]')].some(
    (d) => !state.backdrop || !state.backdrop.contains(d),
  );
}

export function isOpen() {
  return state.backdrop !== null;
}

function row(entry, mac) {
  const r = el("div", "sc-row", "shortcut");
  r.dataset.group = entry.group;
  r.dataset.keys = entry.keys.join(" ");
  const keys = el("span", "sc-keys");
  entry.keys.forEach((acc, i) => {
    if (i > 0) keys.append(el("span", "sc-or", "", " / "));
    const combo = el("span", "sc-combo");
    combo.dataset.testid = "shortcut-keys";
    keyParts(acc, mac).forEach((k, j) => {
      if (j > 0) combo.append("+");
      combo.append(el("kbd", "", "", k));
    });
    keys.append(combo);
  });
  r.append(keys, el("span", "sc-desc", "shortcut-description", entry.description));
  return r;
}

function build() {
  const mac = isMac();
  const backdrop = el("div", "sc-backdrop", "shortcuts-overlay");
  const box = el("div", "sc-dialog", "shortcuts-dialog");
  box.setAttribute("role", "dialog");
  box.setAttribute("aria-modal", "true");
  box.setAttribute("aria-labelledby", "shortcuts-title");
  const head = el("div", "sc-head");
  const title = el("h2", "", "", "Keyboard Shortcuts");
  title.id = "shortcuts-title";
  const close = el("button", "sc-close", "shortcuts-close", "Close");
  close.type = "button";
  close.addEventListener("click", closeShortcuts);
  head.append(title, close);

  const groups = el("div", "sc-groups", "shortcuts-groups");
  // Focusable so the list can scroll with the keyboard in a small window.
  groups.tabIndex = 0;
  for (const g of GROUPS) {
    const entries = SHORTCUTS.filter((s) => s.group === g);
    if (entries.length === 0) continue;
    const sec = el("section", "sc-group", "shortcuts-group");
    sec.dataset.group = g;
    sec.setAttribute("aria-label", g);
    sec.append(el("h3", "", "", g), ...entries.map((e) => row(e, mac)));
    groups.append(sec);
  }
  box.append(head, groups);
  backdrop.append(box);
  backdrop.addEventListener("mousedown", (e) => {
    if (e.target === backdrop) closeShortcuts();
  });
  return backdrop;
}

function focusables() {
  return [...state.backdrop.querySelectorAll("button, [tabindex]")].filter((e) => e.tabIndex >= 0);
}

/** Capture listener while open: the overlay takes every key. */
function onOverlayKey(e) {
  // A prompt opened on top (e.g. Quit asks about unsaved changes) gets its keys.
  if (document.querySelector('[role="alertdialog"]')) return;
  e.stopImmediatePropagation();
  if (e.key === "Escape") {
    e.preventDefault();
    closeShortcuts();
  } else if (e.key === "Tab") {
    const f = focusables();
    if (f.length === 0) return;
    const i = f.indexOf(document.activeElement);
    const next = e.shiftKey ? (i <= 0 ? f.length - 1 : i - 1) : i === f.length - 1 ? 0 : i + 1;
    e.preventDefault();
    f[next].focus();
  }
}

/** Show the overlay (no-op if it or another dialog is already open). */
export function openShortcuts() {
  if (isOpen() || otherDialogOpen()) return;
  state.returnFocus = document.activeElement;
  state.backdrop = build();
  document.body.append(state.backdrop);
  window.addEventListener("keydown", onOverlayKey, true);
  state.backdrop.querySelector(".sc-close").focus();
}

export function closeShortcuts() {
  if (!isOpen()) return;
  window.removeEventListener("keydown", onOverlayKey, true);
  state.backdrop.remove();
  state.backdrop = null;
  const back = state.returnFocus;
  state.returnFocus = null;
  if (back && back.isConnected && typeof back.focus === "function") back.focus();
}

function onKey(e) {
  if (e.key !== "?" || e.metaKey || e.ctrlKey || e.altKey || e.repeat || e.isComposing) return;
  if (e.defaultPrevented || typing(e.target)) return;
  e.preventDefault();
  openShortcuts();
}

/** Wire ? and the Help menu item. */
export function initShortcuts() {
  document.addEventListener("keydown", onKey);
  api.onEvent(MENU_EVENT, (a) => {
    if (a && a.action === MENU_ACTION) openShortcuts();
  });
}
