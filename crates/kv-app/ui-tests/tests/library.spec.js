// Media library: rows from the snapshot, filmstrip URLs, HiLight ticks,
// job spinner and thumbnail swap, skipped-file badges, import (picker and
// drag-and-drop), selection and remove.
//
// library (see crates/kv-app/tests/ui_fixtures.rs): GX010001.MP4 75 s with
// HiLights at 15 s and 60 s, GX010002.MP4 10 s, GX010042.MP4 40 s in two
// chapters with a HiLight at 30 s. library-removed: the same without clip 2.
// import-report: those three added, one photo skipped.

import { test, expect, snapshots } from "../fixtures.js";

const library = snapshots.library;
const removed = snapshots["library-removed"];
const report = snapshots["import-report"];

/** Record every kvthumb:// image the page requests, in order. */
async function recordThumbs(page) {
  await page.addInitScript(() => {
    window.__thumbs = [];
    const desc = Object.getOwnPropertyDescriptor(HTMLImageElement.prototype, "src");
    Object.defineProperty(HTMLImageElement.prototype, "src", {
      ...desc,
      set(v) {
        if (String(v).startsWith("kvthumb://")) window.__thumbs.push(String(v));
        desc.set.call(this, v);
      },
    });
  });
}

const thumbs = (page) => page.evaluate(() => window.__thumbs);
const rows = (page) => page.getByTestId("library-item");
const row = (page, id) => page.locator(`[data-testid=library-item][data-media-id="${id}"]`);

test("items render from the snapshot with name, duration and 12 cells", async ({ page, mock }) => {
  await mock.open(library);
  await expect(rows(page)).toHaveCount(3);
  await expect(rows(page).getByTestId("library-name")).toHaveText([
    "GX010001.MP4",
    "GX010002.MP4",
    "GX010042.MP4",
  ]);
  await expect(rows(page).getByTestId("library-duration")).toHaveText(["1:15", "0:10", "0:40"]);
  for (const id of [1, 2, 3]) {
    await expect(row(page, id).locator(".lib-cell")).toHaveCount(12);
  }
  await expect(row(page, 3)).toHaveAttribute("title", "GX010042.MP4 (2 chapters)");
  // No spinner or error badge without jobs.
  await expect(page.getByTestId("library-spinner").first()).toBeHidden();
  await expect(page.getByTestId("library-error").first()).toBeHidden();
});

test("the empty library has no rows", async ({ page, mock }) => {
  await mock.open(snapshots.initial);
  await expect(page.getByTestId("library-list")).toBeAttached();
  await expect(page.getByTestId("library-import-files")).toBeVisible();
  await expect(rows(page)).toHaveCount(0);
});

test("filmstrip cells request kvthumb://<media_id>/<index>", async ({ page, mock }) => {
  await recordThumbs(page);
  await mock.open(library);
  await expect(rows(page)).toHaveCount(3);
  const cells = row(page, 3).locator(".lib-cell");
  await expect(cells.nth(0)).toHaveAttribute("data-thumb", "kvthumb://3/0");
  await expect(cells.nth(11)).toHaveAttribute("data-thumb", "kvthumb://3/11");
  const want = [1, 2, 3].flatMap((id) => [...Array(12).keys()].map((i) => `kvthumb://${id}/${i}`));
  expect(await thumbs(page)).toEqual(want);
});

test("HiLight ticks sit at hilight / duration over the filmstrip", async ({ page, mock }) => {
  await mock.open(library);
  const fractions = async (id) => {
    const strip = await row(page, id).getByTestId("library-filmstrip").boundingBox();
    const ticks = row(page, id).getByTestId("library-hilight");
    const out = [];
    for (let i = 0; i < (await ticks.count()); i++) {
      const b = await ticks.nth(i).boundingBox();
      out.push((b.x + b.width / 2 - strip.x) / strip.width);
    }
    return out;
  };
  const close = (got, want) => {
    expect(got.length).toBe(want.length);
    got.forEach((g, i) => expect(Math.abs(g - want[i]), `tick ${i}`).toBeLessThan(0.01));
  };
  close(await fractions(1), [15 / 75, 60 / 75]);
  close(await fractions(2), []);
  close(await fractions(3), [30 / 40]);
  await expect(row(page, 1).getByTestId("library-hilight").first()).toHaveAttribute(
    "title",
    "HiLight at 0:15",
  );
});

test("a spinner shows while the filmstrip job runs; Done reloads the images", async ({ page, mock }) => {
  await recordThumbs(page);
  await mock.open(library);
  await expect(rows(page)).toHaveCount(3);
  const spinner = row(page, 2).getByTestId("library-spinner");
  const job = (state, extra = {}) => ({
    id: 7, kind: "filmstrip", media_id: 2, state, fraction: null, message: null, ...extra,
  });
  await mock.event("kv://job", job("queued"));
  await expect(spinner).toBeVisible();
  await expect(row(page, 2)).toHaveAttribute("data-busy", "true");
  await mock.event("kv://job", job("progress", { fraction: 0.5 }));
  await expect(spinner).toBeVisible();
  // Other items are not busy, and other job kinds are ignored.
  await expect(row(page, 1).getByTestId("library-spinner")).toBeHidden();
  await mock.event("kv://job", { ...job("started"), id: 8, kind: "probe", media_id: null });

  const before = (await thumbs(page)).length;
  await mock.event("kv://job", job("done"));
  await expect(spinner).toBeHidden();
  const again = (await thumbs(page)).slice(before);
  expect(again).toEqual([...Array(12).keys()].map((i) => `kvthumb://2/${i}`));
});

test("a failed filmstrip job shows an error badge with the reason", async ({ page, mock }) => {
  await mock.open(library);
  await mock.event("kv://job", {
    id: 3, kind: "filmstrip", media_id: 1, state: "failed", fraction: null,
    message: "Couldn't decode GX010001.MP4.",
  });
  const badge = row(page, 1).getByTestId("library-error");
  await expect(badge).toBeVisible();
  await expect(badge).toHaveAttribute("title", "Couldn't decode GX010001.MP4.");
  await expect(row(page, 1).getByTestId("library-spinner")).toBeHidden();
});

test("Import… runs the file picker and shows skipped files with their reason", async ({ page, mock }) => {
  await mock.open(snapshots.initial);
  await mock.reply("pick_media", report);
  await page.getByTestId("library-import-files").click();
  expect(await mock.commands("pick_media")).toEqual([
    { command: "pick_media", args: { folders: false } },
  ]);
  const skipped = page.getByTestId("library-skipped");
  await expect(skipped).toHaveCount(1);
  await expect(skipped.getByTestId("library-name")).toHaveText("GOPR0007.JPG");
  const badge = skipped.getByTestId("library-error");
  await expect(badge).toBeVisible();
  await expect(badge).toHaveAttribute("title", "Only MP4 and MOV videos can be imported for now.");
  // The backend pushes the new state as kv://state; clips come before errors.
  await mock.state(library);
  await expect(rows(page)).toHaveCount(3);
  await expect(page.locator(".lib-item").last()).toHaveAttribute("data-testid", "library-skipped");
  await skipped.getByTestId("library-dismiss").click();
  await expect(skipped).toHaveCount(0);
});

test("Folder… asks for folders; a cancelled picker changes nothing", async ({ page, mock }) => {
  await mock.open(snapshots.initial);
  await mock.reply("pick_media", null);
  await page.getByTestId("library-import-folder").click();
  expect(await mock.commands("pick_media")).toEqual([
    { command: "pick_media", args: { folders: true } },
  ]);
  await expect(page.getByTestId("library-skipped")).toHaveCount(0);
  await expect(page.getByTestId("toast")).toHaveCount(0);
});

test("an import error shows a toast", async ({ page, mock }) => {
  await mock.open(snapshots.initial);
  await mock.fail("pick_media", "Video import is unavailable: no FFmpeg");
  await page.getByTestId("library-import-files").click();
  await expect(page.getByTestId("toast")).toHaveText("Video import is unavailable: no FFmpeg");
  await expect(page.getByTestId("library-importing")).toBeHidden();
});

test("files dropped on the window are imported", async ({ page, mock }) => {
  await mock.open(snapshots.initial);
  await mock.reply("import", report);
  const paths = ["/home/ana/DCIM/100GOPRO", "/home/ana/clip.mov"];
  await mock.event("tauri://drag-enter", { paths, position: { x: 10, y: 10 } });
  await expect(page.getByTestId("library")).toHaveClass(/lib-drop/);
  await mock.event("tauri://drag-drop", { paths, position: { x: 10, y: 10 } });
  await expect(page.getByTestId("library")).not.toHaveClass(/lib-drop/);
  await expect(page.getByTestId("library-skipped")).toHaveCount(1);
  expect(await mock.commands("import")).toEqual([{ command: "import", args: { paths } }]);
});

test("clicking selects an item and emits kv:media-selected", async ({ page, mock }) => {
  await mock.open(library);
  await page.evaluate(() => {
    window.__selected = [];
    document.addEventListener("kv:media-selected", (e) => window.__selected.push(e.detail.mediaId));
  });
  await row(page, 2).click();
  await expect(row(page, 2)).toHaveAttribute("data-selected", "true");
  await expect(row(page, 1)).toHaveAttribute("data-selected", "false");
  await row(page, 3).click();
  await expect(row(page, 2)).toHaveAttribute("data-selected", "false");
  await expect(row(page, 3)).toHaveAttribute("data-selected", "true");
  expect(await page.evaluate(() => window.__selected)).toEqual([2, 3]);
  const sel = await page.evaluate(async () => (await import("./js/library.js")).selectedMedia());
  expect(sel).toBe(3);
});

const removeCmd = { command: "apply", args: { cmd: { type: "remove_media", media_id: 2 } } };

test("the context menu removes an item; undo brings it back", async ({ page, mock }) => {
  await mock.open(library);
  await row(page, 2).click({ button: "right" });
  await expect(row(page, 2)).toHaveAttribute("data-selected", "true");
  await mock.respond("apply", removed);
  await page.getByTestId("library-remove").click();
  expect(await mock.commands("apply")).toEqual([removeCmd]);
  await expect(rows(page)).toHaveCount(2);
  await expect(page.getByTestId("library-menu")).toHaveCount(0);

  await mock.respond("undo", { ...library, revision: removed.revision + 1 });
  await page.getByTestId("undo").click();
  await expect(rows(page)).toHaveCount(3);
  await expect(row(page, 2).getByTestId("library-name")).toHaveText("GX010002.MP4");
});

test("Delete removes the selected item", async ({ page, mock }) => {
  await mock.open(library);
  await row(page, 2).click();
  await mock.respond("apply", removed);
  await page.keyboard.press("Delete");
  expect(await mock.commands("apply")).toEqual([removeCmd]);
  await expect(rows(page)).toHaveCount(2);
  // The removed item is no longer selected; Delete now does nothing.
  await page.keyboard.press("Backspace");
  expect(await mock.commands("apply")).toHaveLength(1);
});

test("Escape closes the context menu without removing", async ({ page, mock }) => {
  await mock.open(library);
  await row(page, 1).click({ button: "right" });
  await expect(page.getByTestId("library-menu")).toBeVisible();
  await page.keyboard.press("Escape");
  await expect(page.getByTestId("library-menu")).toHaveCount(0);
  expect(await mock.commands("apply")).toHaveLength(0);
});
