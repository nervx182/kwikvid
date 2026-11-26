// Export dialog: options -> request JSON, HEVC hidden when unsupported,
// aspect changes send SetAspectRatio, disabled reasons, and the progress /
// cancel / done / failed states driven by kv://export events.
//
// export-options*, export-done and export-portrait come from real
// kv_session values (crates/kv-app/tests/ui_fixtures.rs). timeline-edit has
// a 4-cut edit at 16:9; initial has no edit.

import { test, expect, snapshots } from "../fixtures.js";

const edit = snapshots["timeline-edit"];
const portrait = snapshots["export-portrait"];
const OPTIONS = snapshots["export-options"];
const H264_ONLY = snapshots["export-options-h264"];
const MISSING = snapshots["export-options-missing"];
const DONE = snapshots["export-done"];
const PATH = "/home/ana/Movies/Untitled.mp4";

/** A kv://export update for export `id`. */
const update = (id, state, extra = {}) => ({ id, state, ...extra });

async function openDialog(page, mock, options = OPTIONS, snapshot = edit) {
  await mock.open(snapshot, { values: { export_options: options } });
  await page.getByTestId("export-open").click();
  await expect(page.getByTestId("export-dialog")).toBeVisible();
  await expect(page.getByTestId("export-start")).toBeEnabled();
}

/** Choose `path` in the save dialog and let start_export answer `id`. */
async function startExport(page, mock, id = 5, path = PATH) {
  await mock.reply("choose_export_path", path);
  await mock.reply("start_export", id);
  await page.getByTestId("export-start").click();
  await expect(page.getByTestId("export-running")).toBeVisible();
  await expect.poll(async () => (await mock.commands("start_export")).length).toBe(1);
}

test("export is disabled with a reason when the edit is empty", async ({ page, mock }) => {
  await mock.open(snapshots.initial);
  const open = page.getByTestId("export-open");
  await expect(open).toBeDisabled();
  await expect(open).toHaveAttribute("title", /Nothing to export yet/);
  await mock.state(edit);
  await expect(open).toBeEnabled();
});

test("dialog is pre-filled from the options and project", async ({ page, mock }) => {
  await openDialog(page, mock);
  expect(await mock.commands("export_options")).toEqual([
    { command: "export_options", args: { section: 1 } },
  ]);
  await expect(page.getByTestId("export-codec").locator("option")).toHaveText([
    "H.264",
    "HEVC (H.265)",
  ]);
  await expect(page.getByTestId("export-resolution")).toHaveValue("1080p");
  await expect(page.getByTestId("export-aspect")).toHaveValue("landscape16x9");
  await expect(page.getByTestId("export-size")).toHaveText("1920 × 1080 · 30 fps");
  await expect(page.getByTestId("export-blocked")).toBeHidden();
});

test("HEVC is hidden when the encoder does not support it", async ({ page, mock }) => {
  await openDialog(page, mock, H264_ONLY);
  await expect(page.getByTestId("export-codec").locator("option")).toHaveText(["H.264"]);
});

test("missing media blocks the export with the reason", async ({ page, mock }) => {
  await mock.open(edit, { values: { export_options: MISSING } });
  await page.getByTestId("export-open").click();
  await expect(page.getByTestId("export-blocked")).toHaveText(MISSING.blocked);
  await expect(page.getByTestId("export-start")).toBeDisabled();
});

for (const [codec, resolution, aspect, size] of [
  ["H264", "1080p", "landscape16x9", "1920 × 1080"],
  ["hevc", "4k", "portrait9x16", "2160 × 3840"],
  ["H264", "4k", "square1x1", "2160 × 2160"],
  ["hevc", "1080p", "square1x1", "1080 × 1080"],
]) {
  test(`request JSON for ${codec} ${resolution} ${aspect}`, async ({ page, mock }) => {
    await openDialog(page, mock);
    await page.getByTestId("export-codec").selectOption(codec);
    await page.getByTestId("export-resolution").selectOption(resolution);
    await page.getByTestId("export-aspect").selectOption(aspect);
    await expect(page.getByTestId("export-size")).toHaveText(`${size} · 30 fps`);
    await startExport(page, mock);
    expect(await mock.commands("choose_export_path")).toEqual([
      { command: "choose_export_path", args: { name: "Untitled.mp4" } },
    ]);
    expect(await mock.commands("start_export")).toEqual([
      {
        command: "start_export",
        args: { section: 1, request: { codec, resolution, aspect, path: PATH } },
      },
    ]);
  });
}

test("changing the aspect sends SetAspectRatio", async ({ page, mock }) => {
  await openDialog(page, mock);
  await mock.respond("apply", portrait);
  await page.getByTestId("export-aspect").selectOption("portrait9x16");
  await expect
    .poll(() => mock.commands("apply"))
    .toEqual([{ command: "apply", args: { cmd: { type: "set_aspect_ratio", aspect: "portrait9x16" } } }]);
  // Picking the project's current aspect sends nothing more.
  await page.getByTestId("export-aspect").selectOption("portrait9x16");
  expect(await mock.commands("apply")).toHaveLength(1);
  // Undo back to 16:9 updates the dialog.
  await mock.state({ ...edit, revision: portrait.revision + 1 });
  await expect(page.getByTestId("export-aspect")).toHaveValue("landscape16x9");
  await expect(page.getByTestId("export-size")).toHaveText("1920 × 1080 · 30 fps");
});

test("cancelling the save dialog starts nothing", async ({ page, mock }) => {
  await openDialog(page, mock);
  await mock.reply("choose_export_path", null);
  await page.getByTestId("export-start").click();
  await expect(page.getByTestId("export-form")).toBeVisible();
  await expect(page.getByTestId("export-start")).toBeEnabled();
  expect(await mock.commands("start_export")).toHaveLength(0);
});

test("progress with ETA, then done with the report and Show in Finder", async ({ page, mock }) => {
  await openDialog(page, mock);
  await startExport(page, mock, 1);
  await expect(page.getByTestId("export-file")).toHaveText("Exporting Untitled.mp4…");
  await mock.event("kv://export", update(1, "started"));
  await mock.event(
    "kv://export",
    update(1, "progress", { frame: 60, total_frames: 240, fraction: 0.25, eta_ms: 65_000 }),
  );
  await expect(page.getByTestId("export-percent")).toHaveText("25%");
  await expect(page.getByTestId("export-eta")).toHaveText("About 1:05 left");
  await expect(page.getByTestId("export-progress")).toHaveJSProperty("value", 0.25);
  // Another export's events are ignored.
  await mock.event("kv://export", update(9, "failed", { message: "not mine" }));
  await expect(page.getByTestId("export-running")).toBeVisible();
  // Escape does not close a running export.
  await page.keyboard.press("Escape");
  await expect(page.getByTestId("export-dialog")).toBeVisible();

  await mock.event("kv://export", DONE);
  await expect(page.getByTestId("export-done")).toBeVisible();
  await expect(page.getByTestId("export-report-file")).toHaveText("Untitled.mp4");
  await expect(page.getByTestId("export-report-duration")).toHaveText("0:08");
  await expect(page.getByTestId("export-report-encoder")).toHaveText("h264_videotoolbox");
  await expect(page.getByTestId("export-report-elapsed")).toHaveText("12.3 s");

  await mock.reply("reveal_export", null);
  await page.getByTestId("export-reveal").click();
  await expect
    .poll(() => mock.commands("reveal_export"))
    .toEqual([{ command: "reveal_export", args: { path: PATH } }]);
  await page.getByTestId("export-done-close").click();
  await expect(page.getByTestId("export-dialog")).toBeHidden();
});

test("cancel sends cancel_export and returns to the options", async ({ page, mock }) => {
  await openDialog(page, mock);
  await startExport(page, mock, 3);
  await mock.event("kv://export", update(3, "progress", { fraction: 0.5, eta_ms: 1000 }));
  await mock.reply("cancel_export", true);
  await page.getByTestId("export-cancel").click();
  await expect
    .poll(() => mock.commands("cancel_export"))
    .toEqual([{ command: "cancel_export", args: { id: 3 } }]);
  await mock.event("kv://export", update(3, "cancelled"));
  await expect(page.getByTestId("export-form")).toBeVisible();
  await expect(page.getByTestId("export-status")).toHaveText(
    "Export cancelled. No file was written.",
  );
  await expect(page.getByTestId("export-start")).toBeEnabled();
});

test("a failed export shows its message", async ({ page, mock }) => {
  await openDialog(page, mock);
  await startExport(page, mock, 4);
  await mock.event("kv://export", update(4, "failed", { message: "The export failed: disk full" }));
  await expect(page.getByTestId("export-error")).toHaveText("The export failed: disk full");
  await page.getByTestId("export-failed-close").click();
  await expect(page.getByTestId("export-dialog")).toBeHidden();
});

test("a start error shows a toast and keeps the options", async ({ page, mock }) => {
  await openDialog(page, mock);
  await mock.reply("choose_export_path", PATH);
  await mock.fail("start_export", "An export is already running.");
  await page.getByTestId("export-start").click();
  await expect(page.getByTestId("toast")).toHaveText("An export is already running.");
  await expect(page.getByTestId("export-form")).toBeVisible();
});

test("Escape and Close dismiss the idle dialog", async ({ page, mock }) => {
  await openDialog(page, mock);
  await expect(page.getByRole("dialog", { name: "Export" })).toBeVisible();
  await page.keyboard.press("Escape");
  await expect(page.getByTestId("export-dialog")).toBeHidden();
  await page.getByTestId("export-open").click();
  await page.getByTestId("export-close").click();
  await expect(page.getByTestId("export-dialog")).toBeHidden();
  // A closed dialog does not block the transport keys.
  await page.keyboard.press("Space");
  await expect.poll(async () => (await mock.commands("play")).length).toBe(1);
});
