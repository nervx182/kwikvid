// First-run welcome panel, centred over the empty viewer: the three steps to
// a first edit ("Add your clips", "Add music", "Pick a length"), each with a
// button that does the same as the existing control, and a tick once done.
// It hides as soon as the default section's edit has segments.
//
// Step state comes only from the snapshot (so undo / redo update it), plus
// kv://job events for "Analysing…" while a music-analysis job runs. A
// regenerate error (snapshot.regen_error) shows under the step it concerns.
// The panel is plain HTML over the transparent viewer (ADR 0007); it is
// absolutely positioned, so it never changes the viewer's rectangle, and it
// stops above the transport bar.

import * as api from "./api.js";

const FINAL = new Set(["done", "failed", "cancelled"]);

const STEPS = [
  {
    id: "clips",
    title: "Add your clips",
    hint: "or drop videos anywhere",
    buttons: [
      ["Import…", "library-import-files"],
      ["Folder…", "library-import-folder"],
    ],
  },
  { id: "music", title: "Add music", hint: "MP3, AAC, WAV or FLAC", buttons: [["Choose file…", "music-choose"]] },
  { id: "length", title: "Pick a length", hint: "15 s, 30 s, 60 s or the full song", buttons: [["Pick…", null]] },
];

/** Music-analysis jobs that have started and not finished. */
const analysing = new Set();
let last = null;

const $ = (testid) => document.querySelector(`[data-testid="${testid}"]`);

function el(tag, className, testid) {
  const e = document.createElement(tag);
  if (className) e.className = className;
  if (testid) e.dataset.testid = testid;
  return e;
}

/** Focus the target-length control: the chosen length, else the first. */
function focusLength() {
  const on = document.querySelector('.ae-lengths [aria-checked="true"]');
  const b = on || document.querySelector(".ae-lengths button");
  if (b) b.focus();
}

function build() {
  const panel = el("div", "wel", "welcome");
  panel.hidden = true;
  const card = el("section", "wel-card");
  card.setAttribute("aria-label", "Getting started");
  const h = el("h2", "wel-title");
  h.textContent = "Make an edit in three steps";
  const ol = el("ol", "wel-steps");
  for (const [i, step] of STEPS.entries()) {
    const li = el("li", "wel-step", `welcome-step-${step.id}`);
    const mark = el("span", "wel-mark");
    mark.textContent = String(i + 1);
    const body = el("div", "wel-body");
    const title = el("span", "wel-step-title");
    title.textContent = step.title;
    const actions = el("div", "wel-actions");
    for (const [label, target] of step.buttons) {
      const b = el("button", "", `welcome-${step.id}-${target || "focus"}`);
      b.type = "button";
      b.textContent = label;
      // Same action as the existing control: click it (a disabled one does nothing).
      b.addEventListener("click", () => (target ? $(target)?.click() : focusLength()));
      actions.append(b);
    }
    const hint = el("span", "wel-hint", `welcome-${step.id}-hint`);
    hint.textContent = step.hint;
    actions.append(hint);
    const error = el("p", "wel-error", `welcome-${step.id}-error`);
    error.setAttribute("role", "alert");
    error.hidden = true;
    body.append(title, actions, error);
    li.append(mark, body);
    ol.append(li);
  }
  card.append(h, ol);
  panel.append(card);
  // Before the transport bar, so tab order and stacking keep it on top.
  $("viewer").prepend(panel);
}

/** Which step an error concerns: by its wording, else the active step. */
export function errorStep(message, active) {
  if (/music|song|beat|analys/i.test(message)) return "music";
  if (/clip|media|video|import/i.test(message)) return "clips";
  if (/length|target|pin/i.test(message)) return "length";
  return active;
}

function onJob(u) {
  if (!u || u.kind !== "music-analysis") return;
  if (FINAL.has(u.state)) analysing.delete(u.id);
  else analysing.add(u.id);
  if (last) renderWelcome(last);
}

export function initWelcome() {
  build();
  api.onEvent("kv://job", onJob);
}

export function renderWelcome(snapshot) {
  last = snapshot;
  const panel = $("welcome");
  if (!panel) return;
  const p = snapshot.project;
  const sec = p.sections[0];
  const done = {
    clips: p.media.length > 0,
    music: !!(sec && sec.music),
    length: !!(sec && sec.edit.segments.length > 0),
  };
  panel.hidden = done.length;
  if (done.length) return;
  const active = STEPS.find((s) => !done[s.id]).id;
  const error = snapshot.regen_error ? errorStep(snapshot.regen_error, active) : null;
  for (const step of STEPS) {
    const li = $(`welcome-step-${step.id}`);
    const state = done[step.id] ? "done" : step.id === active ? "active" : "todo";
    li.dataset.state = state;
    li.querySelector(".wel-mark").textContent = state === "done" ? "✓" : String(STEPS.indexOf(step) + 1);
    const hint = $(`welcome-${step.id}-hint`);
    if (step.id === "music" && analysing.size > 0) hint.textContent = "Analysing…";
    else hint.textContent = step.hint;
    const err = $(`welcome-${step.id}-error`);
    err.hidden = error !== step.id;
    err.textContent = error === step.id ? snapshot.regen_error : "";
  }
}
