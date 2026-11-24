// Error toasts: a rejected command shows its message.

import { test, expect, snapshots } from "../fixtures.js";

test("an error response shows a toast", async ({ page, mock }) => {
  await mock.open(snapshots.edited);
  await mock.fail("undo", "Nothing to undo.");
  await page.getByTestId("undo").click();
  await expect(page.getByTestId("toast")).toHaveText("Nothing to undo.");
  // The failed call left the state alone.
  await expect(page.getByTestId("undo")).toBeEnabled();
});

test("a rejected apply from window.kv shows a toast", async ({ page, mock }) => {
  await mock.open(snapshots.initial);
  await mock.fail("apply", "That part of the project no longer exists.");
  const cmd = { type: "set_seed", section: 99, seed: 1 };
  await page.evaluate((c) => window.kv.apply(c), cmd);
  await expect(page.getByTestId("toast")).toHaveText("That part of the project no longer exists.");
  expect(await mock.commands("apply")).toEqual([{ command: "apply", args: { cmd } }]);
});
