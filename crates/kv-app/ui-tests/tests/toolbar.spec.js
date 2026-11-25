// Toolbar: undo/redo buttons follow can_undo / can_redo and send commands.

import { test, expect, snapshots } from "../fixtures.js";

const CASES = [
  ["initial", false, false],
  ["edited", true, false],
  ["undone", false, true],
  ["undo-redo", true, true],
];

for (const [name, canUndo, canRedo] of CASES) {
  test(`buttons follow can_undo/can_redo: ${name}`, async ({ page, mock }) => {
    const s = snapshots[name];
    expect([s.can_undo, s.can_redo]).toEqual([canUndo, canRedo]);
    await mock.open(s);
    const undo = expect(page.getByTestId("undo"));
    const redo = expect(page.getByTestId("redo"));
    await (canUndo ? undo.toBeEnabled() : undo.toBeDisabled());
    await (canRedo ? redo.toBeEnabled() : redo.toBeDisabled());
  });
}

test("buttons update when the backend pushes a new state", async ({ page, mock }) => {
  await mock.open(snapshots.initial);
  await expect(page.getByTestId("undo")).toBeDisabled();
  await mock.state(snapshots.edited);
  await expect(page.getByTestId("undo")).toBeEnabled();
  await expect(page.getByTestId("redo")).toBeDisabled();
});

test("clicking undo sends undo and renders the result", async ({ page, mock }) => {
  await mock.open(snapshots.edited);
  await mock.respond("undo", snapshots.undone);
  await page.getByTestId("undo").click();
  await expect(page.getByTestId("redo")).toBeEnabled();
  await expect(page.getByTestId("undo")).toBeDisabled();
  expect(await mock.commands("undo")).toEqual([{ command: "undo" }]);
  expect(await mock.commands("redo")).toEqual([]);
});

test("clicking redo sends redo", async ({ page, mock }) => {
  await mock.open(snapshots.undone);
  await mock.respond("redo", snapshots.redone);
  await page.getByTestId("redo").click();
  await expect(page.getByTestId("undo")).toBeEnabled();
  await expect(page.getByTestId("redo")).toBeDisabled();
  expect(await mock.commands("redo")).toEqual([{ command: "redo" }]);
});

test("Ctrl+Z undoes and Ctrl+Shift+Z redoes", async ({ page, mock }) => {
  await mock.open(snapshots["undo-redo"]);
  await page.keyboard.press("Control+z");
  await expect.poll(() => mock.commands("undo")).toHaveLength(1);
  await page.keyboard.press("Control+Shift+z");
  await expect.poll(() => mock.commands("redo")).toHaveLength(1);
});

test("a stale snapshot does not roll the buttons back", async ({ page, mock }) => {
  await mock.open(snapshots["undo-redo"]);
  await expect(page.getByTestId("redo")).toBeEnabled();
  // Older revision than what is on screen: must be ignored.
  await mock.state(snapshots.initial);
  await expect(page.getByTestId("redo")).toBeEnabled();
  await expect(page.getByTestId("undo")).toBeEnabled();
});
