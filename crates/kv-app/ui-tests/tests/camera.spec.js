// Camera / SD card import: the Library's "Import from <card>" button
// follows list_cameras, the dialog lists recordings (already-copied ones
// unticked), start sends the chosen ones, kv://camera events drive the
// progress bar, done closes the dialog, cancel sends cancel_camera_import
// and failures toast.
//
// camera-volumes, camera-recordings and camera-done mirror the backend's
// JSON (crates/kv-app/tests/ui_fixtures.rs; keys checked in src/camera.rs).

import { test, expect, snapshots } from "../fixtures.js";

const VOLUMES = snapshots["camera-volumes"];
const RECORDINGS = snapshots["camera-recordings"];
const DONE = snapshots["camera-done"];
const LIBRARY = "/home/ana/Movies/kwikvid";
const first = (r) => r.files[0];

const update = (id, state, extra = {}) => ({ id, state, ...extra });

async function setCameras(page, volumes) {
  await page.evaluate((v) => (window.__kvMock.values.list_cameras = v), volumes);
  // Detection also runs when the window gains focus.
  await page.evaluate(() => window.dispatchEvent(new Event("focus")));
}

async function openDialog(page, mock) {
  await mock.open(snapshots.initial, {
    values: {
      list_cameras: VOLUMES,
      list_camera_recordings: RECORDINGS,
      camera_library_dir: LIBRARY,
    },
  });
  await page.getByTestId("camera-import").click();
  await expect(page.getByRole("dialog")).toBeVisible();
  await expect(page.getByTestId("camera-recording")).toHaveCount(RECORDINGS.length);
}

async function startImport(page, mock, id = 4) {
  await mock.reply("start_camera_import", id);
  await page.getByTestId("camera-start").click();
  await expect(page.getByTestId("camera-copying")).toBeVisible();
  await expect.poll(async () => (await mock.commands("start_camera_import")).length).toBe(1);
}

test("the button follows mounting and ejecting", async ({ page, mock }) => {
  await mock.open(snapshots.initial, { values: { list_cameras: [] } });
  await expect.poll(async () => (await mock.commands("list_cameras")).length).toBeGreaterThan(0);
  await expect(page.getByTestId("camera-import")).toHaveCount(0);
  await setCameras(page, VOLUMES);
  await expect(page.getByTestId("camera-import")).toHaveText("Import from GOPRO");
  await setCameras(page, []);
  await expect(page.getByTestId("camera-import")).toHaveCount(0);
});

test("the cameras are polled every few seconds", async ({ page, mock }) => {
  await mock.open(snapshots.initial, { values: { list_cameras: [] } });
  await page.evaluate((v) => (window.__kvMock.values.list_cameras = v), VOLUMES);
  await expect(page.getByTestId("camera-import")).toBeVisible({ timeout: 5000 });
});

test("the dialog lists recordings, already-copied ones unticked", async ({ page, mock }) => {
  await openDialog(page, mock);
  expect(await mock.commands("list_camera_recordings")).toEqual([
    { command: "list_camera_recordings", args: { volume: "/Volumes/GOPRO" } },
  ]);
  await expect(page.getByRole("dialog")).toHaveAttribute("aria-labelledby", "camera-title");
  await expect(page.getByTestId("camera-title")).toHaveText("Import from GOPRO");
  await expect(page.getByTestId("camera-name")).toHaveText([
    "GX010042.MP4",
    "GX010043.MP4",
    "GX010044.MP4",
  ]);
  await expect(page.getByTestId("camera-chapters").first()).toHaveText("2 chapters");
  await expect(page.getByTestId("camera-size")).toHaveText(["7.5 GB", "1.1 GB", "537 MB"]);
  const boxes = page.getByTestId("camera-check");
  await expect(boxes.nth(0)).toBeChecked();
  await expect(boxes.nth(1)).not.toBeChecked();
  await expect(boxes.nth(2)).toBeChecked();
  await expect(page.getByTestId("camera-copied")).toHaveCount(1);
  await expect(page.getByTestId("camera-date").first()).not.toHaveText("");
  await expect(page.getByTestId("camera-total")).toHaveText("2 of 3 selected · 8.1 GB");
  await expect(page.getByTestId("camera-dest")).toHaveText(LIBRARY);

  await page.getByTestId("camera-select-none").click();
  await expect(page.getByTestId("camera-total")).toHaveText("0 of 3 selected · 0 bytes");
  await expect(page.getByTestId("camera-start")).toBeDisabled();
  await page.getByTestId("camera-select-all").click();
  await expect(page.getByTestId("camera-total")).toHaveText("3 of 3 selected · 9.1 GB");

  await page.keyboard.press("Escape");
  await expect(page.getByTestId("camera-dialog")).toBeHidden();
  await expect(page.getByRole("dialog")).toHaveCount(0);
});

test("Change… picks the destination and re-lists", async ({ page, mock }) => {
  await openDialog(page, mock);
  await mock.reply("choose_camera_library_dir", "/Volumes/Footage");
  await page.getByTestId("camera-change-dest").click();
  await expect(page.getByTestId("camera-dest")).toHaveText("/Volumes/Footage");
  await expect.poll(async () => (await mock.commands("list_camera_recordings")).length).toBe(2);
  await startImport(page, mock);
  const [start] = await mock.commands("start_camera_import");
  expect(start.args.library_dir).toBe("/Volumes/Footage");
});

test("start sends the chosen recordings, progress moves the bar, done closes", async ({
  page,
  mock,
}) => {
  await openDialog(page, mock);
  // Untick the first, tick the already-copied second.
  await page.getByTestId("camera-check").nth(0).uncheck();
  await page.getByTestId("camera-check").nth(1).check();
  await startImport(page, mock, 1);
  expect(await mock.commands("start_camera_import")).toEqual([
    {
      command: "start_camera_import",
      args: {
        volume: "/Volumes/GOPRO",
        recordings: [first(RECORDINGS[1]), first(RECORDINGS[2])],
        library_dir: LIBRARY,
      },
    },
  ]);
  const total = RECORDINGS[1].total_bytes + RECORDINGS[2].total_bytes;
  await mock.event("kv://camera", update(1, "started", { total_bytes: total, files: 2 }));
  await mock.event(
    "kv://camera",
    update(1, "progress", { bytes_done: total / 4, total_bytes: total, file: first(RECORDINGS[1]) }),
  );
  await expect(page.getByTestId("camera-progress")).toHaveJSProperty("value", 0.25);
  await expect(page.getByTestId("camera-bytes")).toHaveText("403 MB of 1.6 GB");
  await expect(page.getByTestId("camera-eta")).toHaveText(/left|Almost done/);
  // Another import's events are ignored; Escape does not close while copying.
  await mock.event("kv://camera", update(9, "failed", { message: "not mine" }));
  await page.keyboard.press("Escape");
  await expect(page.getByTestId("camera-copying")).toBeVisible();

  await mock.event(
    "kv://camera",
    update(1, "progress", { bytes_done: total, total_bytes: total, file: first(RECORDINGS[2]) }),
  );
  await expect(page.getByTestId("camera-progress")).toHaveJSProperty("value", 1);
  await mock.event("kv://camera", DONE);
  await expect(page.getByTestId("camera-dialog")).toBeHidden();
  await expect(page.getByTestId("toast")).toHaveText("Imported 1 item from GOPRO.");
});

test("cancel sends cancel_camera_import and returns to the list", async ({ page, mock }) => {
  await openDialog(page, mock);
  await startImport(page, mock, 7);
  await mock.reply("cancel_camera_import", true);
  await page.getByTestId("camera-cancel").click();
  await expect
    .poll(() => mock.commands("cancel_camera_import"))
    .toEqual([{ command: "cancel_camera_import", args: { id: 7 } }]);
  await mock.event("kv://camera", update(7, "cancelled"));
  await expect(page.getByTestId("camera-list-view")).toBeVisible();
  await expect(page.getByTestId("camera-status")).toHaveText(
    "Copy cancelled. Files already copied were kept.",
  );
  // Re-listed, so the copies made so far show as copied.
  await expect.poll(async () => (await mock.commands("list_camera_recordings")).length).toBe(2);
});

test("a card removed mid-copy toasts the error", async ({ page, mock }) => {
  await openDialog(page, mock);
  await startImport(page, mock, 2);
  const message =
    '"GOPRO" was removed during the copy. Files already copied were kept; nothing was imported.';
  await mock.event("kv://camera", update(2, "failed", { message }));
  await expect(page.getByTestId("toast")).toHaveText(message);
  await expect(page.getByTestId("camera-list-view")).toBeVisible();
});

test("a start error toasts and keeps the list", async ({ page, mock }) => {
  await openDialog(page, mock);
  await mock.fail("start_camera_import", "A camera import is already running.");
  await page.getByTestId("camera-start").click();
  await expect(page.getByTestId("toast")).toHaveText("A camera import is already running.");
  await expect(page.getByTestId("camera-list-view")).toBeVisible();
});
