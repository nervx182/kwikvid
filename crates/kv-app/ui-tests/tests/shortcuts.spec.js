// Keyboard shortcuts overview (js/shortcuts.js): ? or Help > Keyboard
// Shortcuts (a mock kv://menu event) opens a modal overlay that lists every
// shortcut the feature modules handle, grouped; it takes every key while
// open and changes no state.

import { test, expect, snapshots } from "../fixtures.js";

const GROUPS = ["File", "Edit", "Playback", "Timeline", "Inspector", "View"];
/** Background calls the UI makes on its own (render, title, viewer). */
const QUIET = new Set([
  "get_state",
  "project_info",
  "set_window_title",
  "get_beats",
  "bundled_tracks",
  "set_viewer_rect",
  "get_preview_backend",
  "get_transport",
]);

const overlay = (page) => page.getByTestId("shortcuts-overlay");
const dialog = (page) => page.getByRole("dialog", { name: "Keyboard Shortcuts" });

async function sent(mock) {
  return (await mock.commands()).map((c) => c.command).filter((c) => !QUIET.has(c));
}

test("? opens the overlay and Escape closes it", async ({ page, mock }) => {
  await mock.open(snapshots.initial);
  await expect(overlay(page)).toHaveCount(0);
  await page.keyboard.press("?");
  await expect(dialog(page)).toBeVisible();
  await expect(dialog(page)).toHaveAttribute("aria-modal", "true");
  await expect(page.getByTestId("shortcuts-close")).toBeFocused();
  await page.keyboard.press("Escape");
  await expect(overlay(page)).toHaveCount(0);
});

test("the Help menu action opens it; click outside closes it", async ({ page, mock }) => {
  await mock.open(snapshots.initial);
  await mock.event("kv://menu", { action: "keyboard_shortcuts" });
  await expect(dialog(page)).toBeVisible();
  await page.mouse.click(5, 5); // the backdrop, outside the dialog box
  await expect(overlay(page)).toHaveCount(0);
  await mock.event("kv://menu", { action: "keyboard_shortcuts" });
  await expect(dialog(page)).toBeVisible();
  await page.getByTestId("shortcuts-close").click();
  await expect(overlay(page)).toHaveCount(0);
});

test("every group is present, in two columns", async ({ page, mock }) => {
  await mock.open(snapshots.initial);
  await page.keyboard.press("?");
  const groups = page.getByTestId("shortcuts-group");
  await expect(groups).toHaveCount(GROUPS.length);
  expect(await groups.evaluateAll((gs) => gs.map((g) => g.dataset.group))).toEqual(GROUPS);
  for (const g of GROUPS) {
    await expect(page.getByRole("region", { name: g })).toBeVisible();
  }
  const lefts = await groups.evaluateAll((gs) => gs.map((g) => Math.round(g.getBoundingClientRect().left)));
  expect(new Set(lefts).size).toBe(2);
  // Everything fits at the default window size.
  const box = await dialog(page).boundingBox();
  expect(box.y).toBeGreaterThanOrEqual(0);
  expect(box.y + box.height).toBeLessThanOrEqual(800);
});

test("the overview lists every key the feature modules handle", async ({ page, mock }) => {
  await mock.open(snapshots.initial);
  // The modules' own tables, next to their key handlers.
  const tables = await page.evaluate(async () => {
    const out = {};
    for (const m of ["toolbar", "transport", "tweaks", "inspector"]) {
      out[m] = (await import(`/js/${m}.js`)).KEYS;
    }
    out.menu = (await import("/js/shortcuts.js")).MENU_KEYS;
    return out;
  });
  const expected = Object.values(tables).flat();
  // Every handler the issue names is covered.
  const all = expected.flatMap((e) => e.keys);
  for (const k of [
    "CmdOrCtrl+Z",
    "CmdOrCtrl+Shift+Z",
    "Space",
    "Left",
    "Right",
    "Shift+Left",
    "Shift+Right",
    "Home",
    "End",
    "CmdOrCtrl+Shift+D",
    ",",
    ".",
    "Escape",
    "I",
    "O",
    "CmdOrCtrl+N",
    "CmdOrCtrl+O",
    "CmdOrCtrl+S",
    "CmdOrCtrl+Shift+S",
    "CmdOrCtrl+Q",
    "CmdOrCtrl+/",
    "?",
  ]) {
    expect(all, k).toContain(k);
  }
  await page.keyboard.press("?");
  const rows = await page
    .getByTestId("shortcut")
    .evaluateAll((rs) =>
      rs.map((r) => [r.dataset.group, r.dataset.keys, r.querySelector(".sc-desc").textContent]),
    );
  for (const e of expected) {
    expect(GROUPS).toContain(e.group);
    expect(rows).toContainEqual([e.group, e.keys.join(" "), e.description]);
  }
  expect(rows).toHaveLength(expected.length);
});

test("Cmd on macOS, Ctrl elsewhere", async ({ page, mock }) => {
  await mock.open(snapshots.initial);
  const shown = await page.evaluate(async () => {
    const s = await import("/js/shortcuts.js");
    return {
      mac: s.formatKeys("CmdOrCtrl+Shift+Z", true),
      other: s.formatKeys("CmdOrCtrl+Shift+Z", false),
      arrows: s.formatKeys("Shift+Left", false),
      here: s.isMac(),
    };
  });
  expect(shown.mac).toBe("Cmd+Shift+Z");
  expect(shown.other).toBe("Ctrl+Shift+Z");
  expect(shown.arrows).toBe("Shift+←");
  await page.keyboard.press("?");
  const undo = page.getByTestId("shortcut").filter({ hasText: "Undo" });
  await expect(undo.getByTestId("shortcut-keys")).toHaveText(shown.here ? "Cmd+Z" : "Ctrl+Z");
});

test("opening it changes no state, and other key handlers stay quiet", async ({ page, mock }) => {
  await mock.open(snapshots.edited);
  await expect(page.getByTestId("undo")).toBeEnabled();
  const before = await sent(mock);
  await page.keyboard.press("?");
  await expect(dialog(page)).toBeVisible();
  await mock.event("kv://menu", { action: "keyboard_shortcuts" });
  await expect(overlay(page)).toHaveCount(1);
  // Space (play), arrows (seek), Cmd/Ctrl+Z (undo) and I (mark in) are
  // not acted on while the overlay is open.
  await page.getByTestId("shortcuts-groups").focus();
  for (const k of ["Space", "ArrowRight", "Shift+ArrowLeft", "End", "Control+z", "Meta+z", "i", ","]) {
    await page.keyboard.press(k);
  }
  await expect(dialog(page)).toBeVisible();
  expect(await sent(mock)).toEqual(before);
  await expect(page.getByTestId("transport-play")).toHaveAttribute("data-playing", "false");
  await page.keyboard.press("Escape");
  await expect(overlay(page)).toHaveCount(0);
  expect(await sent(mock)).toEqual(before);
});

test("focus is trapped in the overlay and returns afterwards", async ({ page, mock }) => {
  await mock.open(snapshots.initial);
  await page.getByTestId("transport-play").focus();
  await page.keyboard.press("?");
  for (let i = 0; i < 5; i++) {
    await page.keyboard.press("Tab");
    expect(await page.evaluate(() => !!document.activeElement.closest('[role="dialog"]'))).toBe(true);
  }
  await page.keyboard.press("Shift+Tab");
  expect(await page.evaluate(() => !!document.activeElement.closest('[role="dialog"]'))).toBe(true);
  await page.keyboard.press("Escape");
  await expect(page.getByTestId("transport-play")).toBeFocused();
});

test("typing ? in the title input does not open it", async ({ page, mock }) => {
  await mock.open(snapshots["styles-smooth"]);
  const input = page.getByTestId("style-title");
  await input.click();
  await page.keyboard.type("Why?");
  await expect(input).toHaveValue("Why?");
  await expect(overlay(page)).toHaveCount(0);
});

test("toolbar tooltips name the listed shortcuts", async ({ page, mock }) => {
  await mock.open(snapshots.edited);
  await expect(page.getByTestId("undo")).toHaveAttribute("title", "Undo (Cmd/Ctrl+Z)");
  await expect(page.getByTestId("redo")).toHaveAttribute("title", "Redo (Shift+Cmd/Ctrl+Z)");
  await expect(page.getByTestId("transport-play")).toHaveAttribute("title", "Play (Space)");
  const keysOf = await page.evaluate(async () => {
    const { SHORTCUTS } = await import("/js/shortcuts.js");
    return Object.fromEntries(SHORTCUTS.map((s) => [s.description, s.keys]));
  });
  expect(keysOf["Undo"]).toEqual(["CmdOrCtrl+Z"]);
  expect(keysOf["Redo"]).toEqual(["CmdOrCtrl+Shift+Z"]);
  expect(keysOf["Play / pause"]).toEqual(["Space"]);
});
