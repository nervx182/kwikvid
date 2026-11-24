// The app shell: every layout region is on screen.

import { test, expect, snapshots } from "../fixtures.js";

const REGIONS = ["toolbar", "library", "viewer", "inspector", "timeline"];

test("layout regions are present and visible", async ({ page, mock }) => {
  await mock.open(snapshots.initial);
  await expect(page).toHaveTitle("kwikvid");
  for (const id of REGIONS) {
    await expect(page.getByTestId(id), id).toBeVisible();
  }
});

test("the shell renders the initial snapshot", async ({ page, mock }) => {
  await mock.open(snapshots.initial);
  await expect(page.locator("#library-summary")).toHaveText("No clips yet.");
  await expect(page.locator("#timeline-summary")).toHaveText("No edit yet.");
  await expect(page.getByTestId("inspector")).toContainText("Unsaved");
  expect(await mock.commands("get_state")).toHaveLength(1);
});
