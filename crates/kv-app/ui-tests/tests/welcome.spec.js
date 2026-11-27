// First-run welcome panel over the empty viewer (js/welcome.js).
//
// Fixtures (crates/kv-app/tests/ui_fixtures.rs): initial (empty project),
// timeline-no-music (3 clips, no music), timeline-regen-error (clips and
// music, no edit, regen_error "Still analysing the music…"),
// autoedit-pins-too-long (no edit, a length error), timeline-edit (an edit).

import { test, expect, snapshots } from "../fixtures.js";

const step = (page, id) => page.getByTestId(`welcome-step-${id}`);

test("an empty project shows the panel with step 1 active", async ({ page, mock }) => {
  await mock.open(snapshots.initial);
  await expect(page.getByTestId("welcome")).toBeVisible();
  await expect(step(page, "clips")).toHaveAttribute("data-state", "active");
  await expect(step(page, "music")).toHaveAttribute("data-state", "todo");
  await expect(step(page, "length")).toHaveAttribute("data-state", "todo");
  await expect(page.getByTestId("welcome-clips-hint")).toHaveText("or drop videos anywhere");
  // No promise of built-in music: bring your own.
  await expect(page.getByTestId("welcome")).not.toContainText(/included|bundled|built-in/i);
});

test("clicking step 1 imports like the Library buttons", async ({ page, mock }) => {
  await mock.open(snapshots.initial);
  await mock.reply("pick_media", null);
  await page.getByTestId("welcome-clips-library-import-files").click();
  await expect.poll(() => mock.commands("pick_media")).toEqual([
    { command: "pick_media", args: { folders: false } },
  ]);
  await mock.reply("pick_media", null);
  await page.getByTestId("welcome-clips-library-import-folder").click();
  await expect.poll(async () => (await mock.commands("pick_media")).map((c) => c.args)).toEqual([
    { folders: false },
    { folders: true },
  ]);
});

test("media without music ticks step 1 and makes step 2 active", async ({ page, mock }) => {
  await mock.open(snapshots["timeline-no-music"]);
  await expect(step(page, "clips")).toHaveAttribute("data-state", "done");
  await expect(step(page, "music")).toHaveAttribute("data-state", "active");
  await mock.reply("pick_music", null);
  await page.getByTestId("welcome-music-music-choose").click();
  await expect.poll(async () => (await mock.commands("pick_music")).length).toBe(1);
});

test("step 2 shows Analysing… while the analysis job runs", async ({ page, mock }) => {
  await mock.open(snapshots["timeline-no-music"]);
  const job = (state) => ({ id: 3, kind: "music-analysis", media_id: null, state, fraction: null, message: null });
  await mock.event("kv://job", job("started"));
  await expect(page.getByTestId("welcome-music-hint")).toHaveText("Analysing…");
  await mock.event("kv://job", job("done"));
  await expect(page.getByTestId("welcome-music-hint")).not.toHaveText("Analysing…");
});

test("step 3 focuses the target-length control", async ({ page, mock }) => {
  await mock.open(snapshots["timeline-regen-error"]);
  await expect(step(page, "music")).toHaveAttribute("data-state", "done");
  await expect(step(page, "length")).toHaveAttribute("data-state", "active");
  await page.getByTestId("welcome-length-focus").click();
  await expect(page.locator(".ae-lengths button:focus")).toHaveCount(1);
});

test("a regenerate error shows under the step it concerns", async ({ page, mock }) => {
  await mock.open(snapshots["timeline-regen-error"]);
  await expect(page.getByTestId("welcome-music-error")).toHaveText("Still analysing the music…");
  await expect(page.getByTestId("welcome-length-error")).toBeHidden();
  await mock.state(snapshots["autoedit-pins-too-long"]);
  await expect(page.getByTestId("welcome-length-error")).toHaveText(
    snapshots["autoedit-pins-too-long"].regen_error,
  );
  await expect(page.getByTestId("welcome-music-error")).toBeHidden();
});

test("an edit hides the panel; undo brings it back", async ({ page, mock }) => {
  await mock.open(snapshots["timeline-edit"]);
  await expect(page.getByTestId("welcome")).toBeHidden();
  const back = { ...snapshots["timeline-no-music"], revision: snapshots["timeline-edit"].revision + 1 };
  await mock.state(back);
  await expect(page.getByTestId("welcome")).toBeVisible();
  await expect(step(page, "music")).toHaveAttribute("data-state", "active");
});

test("the panel leaves the transport bar and viewer rect alone", async ({ page, mock }) => {
  await mock.open(snapshots.initial);
  const panel = await page.getByTestId("welcome").boundingBox();
  const transport = await page.getByTestId("transport").boundingBox();
  const viewer = await page.getByTestId("viewer").boundingBox();
  expect(panel.y + panel.height).toBeLessThanOrEqual(transport.y + 0.5);
  await expect.poll(() => mock.commands("set_viewer_rect")).toHaveLength(1);
  // Hiding the panel neither moves nor resizes the viewer.
  await mock.state(snapshots["timeline-edit"]);
  await expect(page.getByTestId("welcome")).toBeHidden();
  expect(await page.getByTestId("viewer").boundingBox()).toEqual(viewer);
  await page.evaluate(() => new Promise((r) => requestAnimationFrame(() => requestAnimationFrame(r))));
  expect(await mock.commands("set_viewer_rect")).toHaveLength(1);
});
