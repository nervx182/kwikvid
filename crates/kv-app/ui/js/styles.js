// Styles in the inspector pane: the preset picker (Cut, Smooth, Action) and
// the title card (text and duration), plus a cheap title preview in the
// viewer.
//
// Choosing a preset sends Command::SetStyle; the backend rebuilds the edit
// with it (kv-session apply_and_regenerate). The title is committed with
// Command::SetTitleCard on blur or Enter (Escape reverts); clearing the text
// sends title: null. Changing the duration commits too, when there is text.
//
// Until the real title render reaches the preview path, the title shows as
// an HTML overlay over the viewer while the playhead is inside the title's
// duration. It follows the timeline playhead (data-position), which the
// transport moves.
//
// Acts on the project's first section (the MVP has one).

import * as api from "./api.js";

const SEC = 1_000_000;

/** Longest title, in characters (kv_core::TitleCard::MAX_CHARS). */
export const TITLE_MAX_CHARS = 80;

/** Title durations offered (guess), in microseconds. */
export const TITLE_DURATIONS = [2 * SEC, 3 * SEC, 4 * SEC];
export const DEFAULT_TITLE_DURATION = 3 * SEC;

const SVG = 'xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" aria-hidden="true"';

/** The presets: kv_core::StylePreset value, label, description, icon. */
export const PRESETS = [
  {
    id: "cut",
    name: "Cut",
    description: "Hard cuts on the beat.",
    icon: `<svg ${SVG}><rect x="2" y="6" width="9" height="12" rx="1"/><rect x="13" y="6" width="9" height="12" rx="1"/></svg>`,
  },
  {
    id: "smooth",
    name: "Smooth",
    description: "Crossfades between clips.",
    icon: `<svg ${SVG}><rect x="2" y="6" width="12" height="12" rx="1" opacity="0.55"/><rect x="10" y="6" width="12" height="12" rx="1" opacity="0.85"/></svg>`,
  },
  {
    id: "action",
    name: "Action",
    description: "Hard cuts, 2x slow motion on HiLights.",
    icon: `<svg ${SVG}><path d="M13 2 4 14h6l-1 8 9-12h-6z"/></svg>`,
  },
];

const state = {
  run: null,
  snapshot: null,
  busy: false,
};

const $ = (id) => document.getElementById(id);

function el(tag, className, testid) {
  const e = document.createElement(tag);
  if (className) e.className = className;
  if (testid) e.dataset.testid = testid;
  return e;
}

function section() {
  return state.snapshot ? state.snapshot.project.sections[0] || null : null;
}

/** The title to send for `text` (null when empty), cut to TITLE_MAX_CHARS characters. */
export function titleFor(text, duration) {
  const t = Array.from(String(text).trim()).slice(0, TITLE_MAX_CHARS).join("");
  return t ? { text: t, duration } : null;
}

function sameTitle(a, b) {
  if (!a || !b) return !a && !b;
  return a.text === b.text && a.duration === b.duration;
}

/** Build the styles DOM and wire it. run(promise) renders its snapshot or toasts its error. */
export function initStyles(run) {
  state.run = run;
  const root = el("section", "sty", "styles");
  root.id = "styles";
  const h = el("h2");
  h.textContent = "Style";

  const picker = el("div", "sty-presets", "style-presets");
  picker.setAttribute("role", "radiogroup");
  picker.setAttribute("aria-label", "Style");
  for (const p of PRESETS) {
    const b = el("button", "sty-preset", `style-${p.id}`);
    b.type = "button";
    b.id = `style-${p.id}`;
    b.setAttribute("role", "radio");
    b.setAttribute("aria-checked", "false");
    const icon = el("span", "sty-icon");
    icon.innerHTML = p.icon; // static markup from PRESETS
    const name = el("span", "sty-name");
    name.textContent = p.name;
    const desc = el("span", "sty-desc");
    desc.textContent = p.description;
    b.append(icon, name, desc);
    b.addEventListener("click", () => setStyle(p.id));
    picker.append(b);
  }

  const titleRow = el("div", "sty-title");
  const label = el("label", "sty-label");
  label.textContent = "Title";
  label.htmlFor = "style-title";
  const input = el("input", "", "style-title");
  input.id = "style-title";
  input.type = "text";
  input.maxLength = TITLE_MAX_CHARS;
  input.placeholder = "Add a title";
  input.autocomplete = "off";
  input.spellcheck = false;
  const duration = el("select", "", "style-title-duration");
  duration.id = "style-title-duration";
  duration.setAttribute("aria-label", "Title duration");
  for (const d of TITLE_DURATIONS) {
    const o = document.createElement("option");
    o.value = String(d);
    o.textContent = `${d / SEC} s`;
    duration.append(o);
  }
  duration.value = String(DEFAULT_TITLE_DURATION);
  titleRow.append(label, input, duration);

  root.append(h, picker, titleRow);
  document.querySelector("[data-testid=inspector]").append(root);

  input.addEventListener("keydown", (e) => {
    if (e.key === "Enter") {
      e.preventDefault();
      commitTitle();
    } else if (e.key === "Escape") {
      e.preventDefault();
      syncTitle(true);
      input.blur();
    }
  });
  input.addEventListener("blur", () => commitTitle());
  duration.addEventListener("change", () => commitTitle());

  const overlay = el("div", "title-overlay", "title-overlay");
  overlay.id = "title-overlay";
  overlay.hidden = true;
  overlay.setAttribute("aria-hidden", "true");
  document.querySelector("[data-testid=viewer]").append(overlay);
  new MutationObserver(drawOverlay).observe($("timeline-playhead"), {
    attributes: true,
    attributeFilter: ["data-position"],
  });
}

/** Apply a command through main.js (render or toast), one at a time. */
async function send(cmd) {
  state.busy = true;
  draw();
  try {
    await state.run(api.apply(cmd));
  } finally {
    state.busy = false;
    draw();
    // A rejected title leaves the field showing what the project has.
    if (document.activeElement !== $("style-title")) syncTitle(true);
  }
}

function setStyle(id) {
  const sec = section();
  if (!sec || state.busy || sec.style === id) return;
  send({ type: "set_style", section: sec.id, style: id });
}

function commitTitle() {
  const sec = section();
  if (!sec || state.busy) return;
  const title = titleFor($("style-title").value, Number($("style-title-duration").value));
  if (sameTitle(title, sec.title)) {
    syncTitle(true);
    return;
  }
  send({ type: "set_title_card", section: sec.id, title });
}

/** Show the project's title in the fields (the text only when `force` or not being edited). */
function syncTitle(force) {
  const sec = section();
  const title = sec ? sec.title : null;
  const input = $("style-title");
  if (force || document.activeElement !== input) input.value = title ? title.text : "";
  if (title && TITLE_DURATIONS.includes(title.duration)) {
    $("style-title-duration").value = String(title.duration);
  } else if (title) {
    // A duration set elsewhere (CLI, MCP): offer it as it is.
    const d = $("style-title-duration");
    if (![...d.options].some((o) => o.value === String(title.duration))) {
      const o = document.createElement("option");
      o.value = String(title.duration);
      o.textContent = `${title.duration / SEC} s`;
      d.append(o);
    }
    d.value = String(title.duration);
  }
}

function drawOverlay() {
  const sec = section();
  const overlay = $("title-overlay");
  const title = sec ? sec.title : null;
  const position = Number($("timeline-playhead").dataset.position || 0);
  const show = !!title && position < title.duration;
  overlay.hidden = !show;
  overlay.textContent = show ? title.text : "";
}

function draw() {
  const sec = section();
  for (const p of PRESETS) {
    const b = $(`style-${p.id}`);
    const on = !!sec && sec.style === p.id;
    b.setAttribute("aria-checked", String(on));
    b.classList.toggle("on", on);
    b.disabled = !sec || state.busy;
  }
  $("style-title").disabled = !sec;
  $("style-title-duration").disabled = !sec;
  drawOverlay();
}

export function renderStyles(snapshot) {
  state.snapshot = snapshot;
  syncTitle(false);
  draw();
}
