// Entry point: keeps the latest Snapshot and re-renders the shell from it.
// Features add their own module (js/<feature>.js) and a one-line hook here.

import * as api from "./api.js";
import { showToast } from "./toast.js";
import { initToolbar, renderToolbar } from "./toolbar.js";
import { initProject, renderProject } from "./project.js";
import { initTimeline, renderTimeline } from "./timeline.js";
import { initTweaks } from "./tweaks.js";
import { initAutoEdit, renderAutoEdit } from "./autoedit.js";
import { initViewer } from "./viewer.js";
import { initLibrary, renderLibrary } from "./library.js";
import { initInspector, renderInspector } from "./inspector.js";
import { initTransport, renderTransport } from "./transport.js";
import { initStyles, renderStyles } from "./styles.js";
import { initMusic, renderMusic } from "./music.js";

let current = null;

function render(snapshot) {
  // Snapshots can arrive twice (command result and kv://state event) or out
  // of order; never go back to an older revision.
  if (current && snapshot.revision < current.revision) return;
  current = snapshot;
  renderToolbar(snapshot);
  renderTimeline(snapshot);
  renderTransport(snapshot);
  renderAutoEdit(snapshot);
  renderLibrary(snapshot);
  renderInspector(snapshot);
  renderStyles(snapshot);
  renderMusic(snapshot);
  renderSummary(snapshot);
  renderProject(snapshot);
}

function renderSummary(s) {
  const p = s.project;
  const media = p.media.length;
  document.getElementById("library-summary").textContent =
    media === 0 ? "No clips yet." : `${media} item${media === 1 ? "" : "s"}`;
  const cuts = p.sections.reduce((n, sec) => n + sec.edit.segments.length, 0);
  document.getElementById("timeline-summary").textContent =
    cuts === 0 ? "No edit yet." : `${cuts} cut${cuts === 1 ? "" : "s"}`;
  const dl = document.getElementById("inspector-summary");
  dl.replaceChildren();
  for (const [k, v] of [
    ["Revision", s.revision],
    ["Sections", p.sections.length],
    ["Unsaved", s.dirty ? "yes" : "no"],
  ]) {
    const dt = document.createElement("dt");
    dt.textContent = k;
    const dd = document.createElement("dd");
    dd.textContent = String(v);
    dl.append(dt, dd);
  }
}

/** Await a command; render its snapshot or toast its error. */
async function run(promise) {
  try {
    render(await promise);
  } catch (e) {
    showToast(e && e.message ? e.message : String(e));
  }
}

initToolbar(run);
initProject(render);
initTimeline(run);
initTweaks(run);
initTransport(run);
initAutoEdit(run, render);
initLibrary(run);
initInspector(render);
initStyles(run);
initMusic(run);
initViewer(document.querySelector('[data-testid="viewer"]'), api.setViewerRect);
api.onState(render);
run(api.getState());

// For the console and later UI tests: window.kv.apply({ type: "set_seed", section: 1, seed: 7 }).
window.kv = { apply: (cmd) => run(api.apply(cmd)) };
