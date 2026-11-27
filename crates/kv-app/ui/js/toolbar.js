// Toolbar: undo/redo buttons and their keyboard shortcuts.

import * as api from "./api.js";

// The keys the handler below acts on, for the shortcuts overview (shortcuts.js).
// Notation: Tauri accelerators (CmdOrCtrl = Cmd on macOS, Ctrl elsewhere).
export const KEYS = [
  { group: "Edit", keys: ["CmdOrCtrl+Z"], description: "Undo" },
  { group: "Edit", keys: ["CmdOrCtrl+Shift+Z"], description: "Redo" },
];

const undoBtn = () => document.getElementById("undo");
const redoBtn = () => document.getElementById("redo");

/** Wire the buttons and shortcuts. run(promise) handles errors. */
export function initToolbar(run) {
  undoBtn().addEventListener("click", () => run(api.undo()));
  redoBtn().addEventListener("click", () => run(api.redo()));
  document.addEventListener("keydown", (e) => {
    if (!(e.metaKey || e.ctrlKey) || e.altKey || e.key.toLowerCase() !== "z") return;
    const t = e.target;
    if (t && (t.isContentEditable || ["INPUT", "TEXTAREA", "SELECT"].includes(t.tagName))) return;
    e.preventDefault();
    if (e.shiftKey) {
      if (!redoBtn().disabled) run(api.redo());
    } else if (!undoBtn().disabled) {
      run(api.undo());
    }
  });
}

export function renderToolbar(snapshot) {
  undoBtn().disabled = !snapshot.can_undo;
  redoBtn().disabled = !snapshot.can_redo;
}
