// Auto-edit controls in the toolbar: target length (15/30/60 s or the full
// song), re-roll (button or R), the seed, a spinner while the edit is being
// rebuilt, rebuild errors inline, and a "Manual edits replaced" toast with
// Undo.
//
// The controls act on the project's first section (the MVP has one). The
// backend rebuilds the edit when the target length changes (kv-session
// apply_and_regenerate) and answers with a snapshot that may carry
// regen_error and manual_edits_replaced.

import * as api from "./api.js";
import { showToast } from "./toast.js";

const SEC = 1_000_000;

/** Show the spinner when a rebuild takes longer than this. */
export const SPINNER_DELAY_MS = 100;

/** The length choices and the TargetLength each one sends. */
export const LENGTHS = [
  { id: "15", label: "15 s", target: { type: "fixed", length: 15 * SEC } },
  { id: "30", label: "30 s", target: { type: "fixed", length: 30 * SEC } },
  { id: "60", label: "60 s", target: { type: "fixed", length: 60 * SEC } },
  { id: "full", label: "Full song", target: { type: "full_song" } },
];

const REROLL_TITLE = "Re-roll: a new edit with the same moments forced in (R)";

let run = null; // main.js: await a promise, render its snapshot or toast its error
let render = null; // main.js: render a snapshot
let current = null; // the latest snapshot
let busy = 0; // commands in flight
let spinnerTimer = null;
let actionError = null; // { revision, message } of the last rejected action
let toastedRevision = -1;

const el = (id) => document.getElementById(id);

function sameTarget(a, b) {
  return a.type === b.type && (a.type !== "fixed" || a.length === b.length);
}

function build() {
  const bar = document.createElement("div");
  bar.className = "autoedit";
  bar.dataset.testid = "autoedit";

  const lengths = document.createElement("div");
  lengths.className = "ae-lengths";
  lengths.setAttribute("role", "radiogroup");
  lengths.setAttribute("aria-label", "Edit length");
  for (const l of LENGTHS) {
    const b = document.createElement("button");
    b.type = "button";
    b.id = `autoedit-length-${l.id}`;
    b.dataset.testid = `autoedit-length-${l.id}`;
    b.setAttribute("role", "radio");
    b.setAttribute("aria-checked", "false");
    b.textContent = l.label;
    b.addEventListener("click", () => setLength(l));
    lengths.append(b);
  }

  const reroll = document.createElement("button");
  reroll.type = "button";
  reroll.id = "autoedit-reroll";
  reroll.dataset.testid = "autoedit-reroll";
  reroll.textContent = "Re-roll";
  reroll.disabled = true;
  reroll.addEventListener("click", () => doReroll());

  const spinner = document.createElement("span");
  spinner.id = "autoedit-spinner";
  spinner.className = "ae-spinner";
  spinner.dataset.testid = "autoedit-spinner";
  spinner.setAttribute("role", "progressbar");
  spinner.setAttribute("aria-label", "Building the edit");
  spinner.hidden = true;

  const seed = document.createElement("span");
  seed.id = "autoedit-seed";
  seed.className = "ae-seed";
  seed.dataset.testid = "autoedit-seed";
  seed.title = "Seed of this edit (include it in bug reports)";

  const error = document.createElement("span");
  error.id = "autoedit-error";
  error.className = "ae-error";
  error.dataset.testid = "autoedit-error";
  error.setAttribute("role", "alert");
  error.hidden = true;

  bar.append(lengths, reroll, spinner, seed, error);
  document.querySelector(".toolbar").append(bar);
}

function section() {
  return current && current.project.sections[0];
}

/** Why re-roll is unavailable, or null when it is. */
function rerollBlocker(s) {
  if (!s || !section()) return "Nothing to re-roll yet.";
  if (s.project.media.length === 0) return "Add clips to re-roll the edit.";
  if (!section().music) return "Add music to re-roll the edit.";
  return null;
}

/** Run an auto-edit command: spinner after a delay, errors shown inline. */
async function send(promise) {
  busy += 1;
  if (busy === 1) {
    spinnerTimer = setTimeout(() => (el("autoedit-spinner").hidden = false), SPINNER_DELAY_MS);
  }
  try {
    const snapshot = await promise;
    actionError = null;
    render(snapshot);
  } catch (e) {
    actionError = {
      revision: current ? current.revision : -1,
      message: e && e.message ? e.message : String(e),
    };
    renderError();
  } finally {
    busy -= 1;
    if (busy === 0) {
      clearTimeout(spinnerTimer);
      el("autoedit-spinner").hidden = true;
    }
  }
}

function setLength(l) {
  const sec = section();
  if (!sec || busy > 0 || sameTarget(sec.target_length, l.target)) return;
  send(api.apply({ type: "set_target_length", section: sec.id, target: l.target }));
}

function doReroll() {
  if (busy > 0 || rerollBlocker(current)) return;
  send(api.reroll(section().id));
}

function renderError() {
  const box = el("autoedit-error");
  let message = null;
  if (actionError && current && actionError.revision === current.revision) {
    message = actionError.message;
  } else if (current && current.regen_error) {
    message = current.regen_error;
  }
  box.textContent = message || "";
  box.hidden = !message;
}

/** Wire the controls. run(promise) renders or toasts; render(snapshot) renders. */
export function initAutoEdit(runFn, renderFn) {
  run = runFn;
  render = renderFn;
  build();
  document.addEventListener("keydown", (e) => {
    if (e.key.toLowerCase() !== "r" || e.metaKey || e.ctrlKey || e.altKey || e.repeat) return;
    const t = e.target;
    if (t && (t.isContentEditable || ["INPUT", "TEXTAREA", "SELECT"].includes(t.tagName))) return;
    e.preventDefault();
    doReroll();
  });
}

export function renderAutoEdit(snapshot) {
  current = snapshot;
  const sec = section();
  for (const l of LENGTHS) {
    const on = !!sec && sameTarget(sec.target_length, l.target);
    const b = el(`autoedit-length-${l.id}`);
    b.setAttribute("aria-checked", String(on));
    b.classList.toggle("on", on);
    b.disabled = !sec;
  }
  const blocker = rerollBlocker(snapshot);
  const reroll = el("autoedit-reroll");
  reroll.disabled = !!blocker;
  reroll.title = blocker || REROLL_TITLE;
  el("autoedit-seed").textContent = sec ? `seed ${sec.seed}` : "";
  renderError();

  // The same snapshot can arrive twice (command result and kv://state).
  if (snapshot.manual_edits_replaced && snapshot.revision > toastedRevision) {
    toastedRevision = snapshot.revision;
    showToast("Manual edits replaced", {
      kind: "info",
      action: { label: "Undo", onClick: () => run(api.undo()) },
    });
  }
}
