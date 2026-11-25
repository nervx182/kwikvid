// Auto-edit controls: target length, re-roll, seed, spinner, inline errors
// and the "Manual edits replaced" toast with Undo.

import { test, expect, snapshots } from "../fixtures.js";

const SEC = 1_000_000;
const ready = snapshots["autoedit-ready"];

/** `s` at a newer revision (so the UI does not drop it as stale). */
function newer(s, revision) {
  return { ...structuredClone(s), revision };
}

const LENGTHS = [
  ["15", { type: "fixed", length: 15 * SEC }],
  ["30", { type: "fixed", length: 30 * SEC }],
  ["60", { type: "fixed", length: 60 * SEC }],
  ["full", { type: "full_song" }],
];

test("the fixtures say what the tests assume", () => {
  const sec = ready.project.sections[0];
  expect(sec.target_length).toEqual({ type: "fixed", length: 15 * SEC });
  expect(sec.seed).toBe(0);
  expect(snapshots["autoedit-rerolled"].project.sections[0].seed).toBe(1);
  expect(snapshots["autoedit-manual-replaced"].manual_edits_replaced).toBe(true);
  expect(snapshots["autoedit-pins-too-long"].regen_error).toBe(
    "A pinned moment is longer than the target length.",
  );
});

test("the current length is selected and the seed shown", async ({ page, mock }) => {
  await mock.open(ready);
  await expect(page.getByTestId("autoedit-length-15")).toHaveAttribute("aria-checked", "true");
  for (const id of ["30", "60", "full"]) {
    await expect(page.getByTestId(`autoedit-length-${id}`)).toHaveAttribute("aria-checked", "false");
  }
  await expect(page.getByTestId("autoedit-seed")).toHaveText("seed 0");
  await expect(page.getByTestId("autoedit-reroll")).toBeEnabled();
  await expect(page.getByTestId("autoedit-spinner")).toBeHidden();
  await expect(page.getByTestId("autoedit-error")).toBeHidden();
});

test("a new project defaults to the full song", async ({ page, mock }) => {
  await mock.open(snapshots.initial);
  await expect(page.getByTestId("autoedit-length-full")).toHaveAttribute("aria-checked", "true");
});

for (const [id, target] of LENGTHS) {
  test(`choosing ${id} sends set_target_length`, async ({ page, mock }) => {
    // Start from a length other than the one clicked.
    const start = id === "15" ? snapshots.initial : ready;
    await mock.open(start);
    await page.getByTestId(`autoedit-length-${id}`).click();
    await expect.poll(() => mock.commands("apply")).toEqual([
      { command: "apply", args: { cmd: { type: "set_target_length", section: 1, target } } },
    ]);
  });
}

test("clicking the current length sends nothing", async ({ page, mock }) => {
  await mock.open(ready);
  await page.getByTestId("autoedit-length-15").click();
  await page.waitForTimeout(50);
  expect(await mock.commands("apply")).toEqual([]);
});

test("the selection follows the answer", async ({ page, mock }) => {
  await mock.open(ready);
  await mock.respond("apply", snapshots["autoedit-manual-replaced"]);
  await page.getByTestId("autoedit-length-30").click();
  await expect(page.getByTestId("autoedit-length-30")).toHaveAttribute("aria-checked", "true");
  await expect(page.getByTestId("autoedit-length-15")).toHaveAttribute("aria-checked", "false");
});

test("re-roll sends reroll and shows the new seed", async ({ page, mock }) => {
  await mock.open(ready);
  await mock.respond("reroll", snapshots["autoedit-rerolled"]);
  await page.getByTestId("autoedit-reroll").click();
  await expect(page.getByTestId("autoedit-seed")).toHaveText("seed 1");
  expect(await mock.commands("reroll")).toEqual([{ command: "reroll", args: { section: 1 } }]);
  expect(await mock.commands("apply")).toEqual([]);
});

test("R re-rolls; Cmd/Ctrl+R does not", async ({ page, mock }) => {
  await mock.open(ready);
  await page.keyboard.press("r");
  await expect.poll(() => mock.commands("reroll")).toHaveLength(1);
  await page.keyboard.press("Control+r").catch(() => {});
  await page.waitForTimeout(50);
  expect(await mock.commands("reroll")).toHaveLength(1);
});

test("re-roll is disabled with a reason when there are no clips or no music", async ({
  page,
  mock,
}) => {
  await mock.open(snapshots.initial);
  const reroll = page.getByTestId("autoedit-reroll");
  await expect(reroll).toBeDisabled();
  await expect(reroll).toHaveAttribute("title", "Add clips to re-roll the edit.");
  await page.keyboard.press("r");

  await mock.state(snapshots["timeline-no-music"]);
  await expect(reroll).toBeDisabled();
  await expect(reroll).toHaveAttribute("title", "Add music to re-roll the edit.");
  await page.keyboard.press("r");

  await mock.state(ready);
  await expect(reroll).toBeEnabled();
  await expect(reroll).toHaveAttribute("title", /^Re-roll/);
  expect(await mock.commands("reroll")).toEqual([]);
});

test("a slow rebuild shows the spinner; a fast one does not", async ({ page, mock }) => {
  await mock.open(ready);
  const spinner = page.getByTestId("autoedit-spinner");

  await mock.respond("reroll", snapshots["autoedit-rerolled"], { delayMs: 600 });
  await page.getByTestId("autoedit-reroll").click();
  await expect(spinner).toBeVisible();
  await expect(page.getByTestId("autoedit-seed")).toHaveText("seed 1");
  await expect(spinner).toBeHidden();

  // An instant answer never shows it.
  await page.evaluate(() => {
    window.__spinnerSeen = false;
    const s = document.getElementById("autoedit-spinner");
    new MutationObserver(() => {
      if (!s.hidden) window.__spinnerSeen = true;
    }).observe(s, { attributes: true });
  });
  await mock.respond("apply", snapshots["autoedit-manual-replaced"]);
  await page.getByTestId("autoedit-length-30").click();
  await expect(page.getByTestId("autoedit-length-30")).toHaveAttribute("aria-checked", "true");
  await page.waitForTimeout(200);
  expect(await page.evaluate(() => window.__spinnerSeen)).toBe(false);
});

test("manual edits replaced: a toast with Undo", async ({ page, mock }) => {
  await mock.open(ready);
  const replaced = snapshots["autoedit-manual-replaced"];
  await mock.respond("apply", replaced);
  await page.getByTestId("autoedit-length-30").click();
  const toast = page.getByTestId("toast");
  await expect(toast).toHaveCount(1);
  await expect(toast).toContainText("Manual edits replaced");

  // The same snapshot pushed again (kv://state) does not toast twice.
  await mock.state(replaced);
  await page.waitForTimeout(50);
  await expect(toast).toHaveCount(1);

  await mock.respond("undo", newer(ready, replaced.revision + 1));
  await toast.getByTestId("toast-action").click();
  await expect(toast).toHaveCount(0);
  expect(await mock.commands("undo")).toEqual([{ command: "undo" }]);
  await expect(page.getByTestId("autoedit-length-15")).toHaveAttribute("aria-checked", "true");
});

test("no toast when nothing manual was replaced", async ({ page, mock }) => {
  await mock.open(ready);
  await mock.respond("reroll", snapshots["autoedit-rerolled"]);
  await page.getByTestId("autoedit-reroll").click();
  await expect(page.getByTestId("autoedit-seed")).toHaveText("seed 1");
  await expect(page.getByTestId("toast")).toHaveCount(0);
});

test("a rebuild error from a length change shows inline", async ({ page, mock }) => {
  await mock.open(ready);
  const tooLong = snapshots["autoedit-pins-too-long"];
  await mock.respond("apply", newer(tooLong, ready.revision + 1));
  await page.getByTestId("autoedit-length-60").click();
  const error = page.getByTestId("autoedit-error");
  await expect(error).toBeVisible();
  await expect(error).toHaveText("A pinned moment is longer than the target length.");
  await expect(page.getByTestId("toast")).toHaveCount(0);

  // The next snapshot without an error clears it.
  await mock.state(newer(ready, ready.revision + 2));
  await expect(error).toBeHidden();
});

test("a rejected re-roll shows inline until the state changes", async ({ page, mock }) => {
  await mock.open(ready);
  await mock.fail("reroll", "Still analysing the music…");
  await page.getByTestId("autoedit-reroll").click();
  const error = page.getByTestId("autoedit-error");
  await expect(error).toHaveText("Still analysing the music…");
  await expect(page.getByTestId("toast")).toHaveCount(0);
  await expect(page.getByTestId("autoedit-seed")).toHaveText("seed 0");

  await mock.state(snapshots["autoedit-rerolled"]);
  await expect(error).toBeHidden();
});
