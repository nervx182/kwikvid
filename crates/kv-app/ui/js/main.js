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
import { initViewerWebGl, renderViewerWebGl } from "./viewer-webgl.js";
import { initLibrary, renderLibrary } from "./library.js";
import { initInspector, renderInspector } from "./inspector.js";
import { initTransport, renderTransport } from "./transport.js";
import { initStyles, renderStyles } from "./styles.js";
import { initMusic, renderMusic } from "./music.js";
import { initExport, renderExport } from "./export.js";
import { initCamera } from "./camera.js";
import { initShortcuts } from "./shortcuts.js";
import { initWelcome, renderWelcome } from "./welcome.js";

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
  renderExport(snapshot);
  renderSummary(snapshot);
  renderProject(snapshot);
  renderViewerWebGl(snapshot);
  renderWelcome(snapshot);
}

function renderSummary(s) {
  const p = s.project;
  const media = p.media.length;
  document.getElementById("library-summary").textContent =
    media === 0 ? "No clips yet." : `${media} item${media === 1 ? "" : "s"}`;
  const cuts = p.sections.reduce((n, sec) => n + sec.edit.segments.length, 0);
  document.getElementById("timeline-summary").textContent =
    cuts === 0 ? "No edit yet." : `${cuts} cut${cuts === 1 ? "" : "s"}`;
  // No clip selected: a hint. The debug values live in the preview debug
  // overlay (Shift+Cmd/Ctrl+D, transport.js).
  document.getElementById("inspector-summary").textContent = "Select a clip to see it here";
  document.getElementById("preview-debug").dataset.state =
    `revision ${s.revision} · sections ${p.sections.length} · unsaved ${s.dirty ? "yes" : "no"}`;
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
initCamera();
initInspector(render);
initStyles(run);
initMusic(run);
initExport(run);
initShortcuts();
initWelcome();
initViewer(document.querySelector('[data-testid="viewer"]'), api.setViewerRect);
initViewerWebGl(document.querySelector('[data-testid="viewer"]'), api);
api.onState(render);
run(api.getState());

// For the console and later UI tests: window.kv.apply({ type: "set_seed", section: 1, seed: 7 }).
window.kv = { apply: (cmd) => run(api.apply(cmd)) };
