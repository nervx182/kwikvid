// Project files: File menu actions (sent as mock kv://menu events) map to
// project commands; window title with edited marker; Save / Don't Save /
// Cancel prompt; missing-media banner.

import { test, expect, snapshots } from "../fixtures.js";

const MENU = "kv://menu";
const TRIP = "/home/ana/Movies/trip/trip.kwikvid";
const CLIP = (n) => `/home/ana/Movies/trip/clips/GX01000${n}.MP4`;

/** A project command's result, as src/project.rs returns it. */
const result = (snapshot, file = null, missing = []) => ({ snapshot, file, missing });

const missingClips = [1, 2, 3].map((n) => ({
  kind: { type: "media", media_id: n },
  project_path: `clips/GX01000${n}.MP4`,
  resolved: CLIP(n),
}));

/** Names of the commands the UI sent, in order, ignoring rendering calls. */
async function flow(mock) {
  const quiet = new Set(["get_state", "project_info", "set_window_title"]);
  return (await mock.commands()).map((c) => c.command).filter((c) => !quiet.has(c));
}

const menu = (mock, action, extra = {}) => mock.event(MENU, { action, ...extra });

test.describe("window title", () => {
  test("untitled: app name, bullet when edited", async ({ page, mock }) => {
    await mock.open(snapshots.initial);
    await expect(page).toHaveTitle("kwikvid");
    await mock.state(snapshots.edited);
    await expect(page).toHaveTitle("• kwikvid");
    await expect
      .poll(async () => (await mock.commands("set_window_title")).map((c) => c.args.title))
      .toEqual(["kwikvid", "• kwikvid"]);
  });

  test("titled: project name, bullet follows dirty", async ({ page, mock }) => {
    await mock.open(snapshots.saved, {
      values: { project_info: result(snapshots.saved, TRIP) },
    });
    await expect(page).toHaveTitle("trip — kwikvid");
    // An edit after the save (newer revision, dirty).
    await mock.state({ ...snapshots.saved, revision: snapshots.saved.revision + 1, dirty: true });
    await expect(page).toHaveTitle("• trip — kwikvid");
    await expect
      .poll(async () => (await mock.commands("set_window_title")).at(-1).args)
      .toEqual({ title: "• trip — kwikvid" });
  });
});

test.describe("menu actions", () => {
  test("New with no changes creates a project without asking", async ({ page, mock }) => {
    await mock.open(snapshots.initial);
    await mock.reply("new_project", result(snapshots.edited));
    await menu(mock, "new");
    await expect.poll(() => flow(mock)).toEqual(["new_project"]);
    await expect(page.getByTestId("unsaved-prompt")).toHaveCount(0);
  });

  test("Open picks a file and opens it", async ({ page, mock }) => {
    await mock.open(snapshots.initial);
    await mock.reply("choose_open_path", TRIP);
    await mock.reply("open_project", result(snapshots.opened, TRIP));
    await menu(mock, "open");
    await expect.poll(() => flow(mock)).toEqual(["choose_open_path", "open_project"]);
    expect((await mock.commands("open_project"))[0].args).toEqual({ path: TRIP });
    await expect(page.locator("#library-summary")).toHaveText("3 items");
    await expect(page).toHaveTitle("trip — kwikvid");
    await expect(page.getByTestId("missing-media")).toHaveCount(0);
  });

  test("Open cancelled in the picker does nothing", async ({ mock }) => {
    await mock.open(snapshots.initial);
    await mock.reply("choose_open_path", null);
    await menu(mock, "open");
    await expect.poll(() => flow(mock)).toEqual(["choose_open_path"]);
  });

  test("Open Recent opens its path without a picker", async ({ mock }) => {
    await mock.open(snapshots.initial);
    await mock.reply("open_project", result(snapshots.opened, TRIP));
    await menu(mock, "open_recent", { path: TRIP });
    await expect.poll(() => flow(mock)).toEqual(["open_project"]);
    expect((await mock.commands("open_project"))[0].args).toEqual({ path: TRIP });
  });

  test("Save on an untitled project acts as Save As", async ({ page, mock }) => {
    await mock.open(snapshots.edited);
    await mock.reply("choose_save_path", "/home/ana/trip");
    await mock.reply("save_project_as", result(snapshots.saved, TRIP));
    await menu(mock, "save");
    await expect.poll(() => flow(mock)).toEqual(["choose_save_path", "save_project_as"]);
    expect((await mock.commands("save_project_as"))[0].args).toEqual({ path: "/home/ana/trip" });
    await expect(page).toHaveTitle("trip — kwikvid");
  });

  test("Save on a titled project saves in place", async ({ page, mock }) => {
    const dirty = { ...snapshots.saved, revision: snapshots.saved.revision + 1, dirty: true };
    await mock.open(dirty, { values: { project_info: result(dirty, TRIP) } });
    await expect(page).toHaveTitle("• trip — kwikvid");
    await mock.reply("save_project", result({ ...dirty, dirty: false }, TRIP));
    await menu(mock, "save");
    await expect.poll(() => flow(mock)).toEqual(["save_project"]);
    await expect(page).toHaveTitle("trip — kwikvid");
  });

  test("Save As always asks for a location; cancel saves nothing", async ({ mock }) => {
    await mock.open(snapshots.saved, { values: { project_info: result(snapshots.saved, TRIP) } });
    await mock.reply("choose_save_path", null);
    await menu(mock, "save_as");
    await expect.poll(() => flow(mock)).toEqual(["choose_save_path"]);
  });

  test("a failed save shows a toast", async ({ page, mock }) => {
    await mock.open(snapshots.saved, { values: { project_info: result(snapshots.saved, TRIP) } });
    await expect(page).toHaveTitle("trip — kwikvid");
    await mock.fail("save_project", "The project file couldn't be read or written.");
    await menu(mock, "save");
    await expect(page.getByTestId("toast")).toHaveText("The project file couldn't be read or written.");
  });
});

test.describe("unsaved-changes prompt", () => {
  test("Cancel keeps the project", async ({ page, mock }) => {
    await mock.open(snapshots.edited);
    await menu(mock, "new");
    const prompt = page.getByTestId("unsaved-prompt");
    await expect(prompt).toBeVisible();
    await expect(prompt).toContainText("Untitled");
    await page.getByTestId("prompt-cancel").click();
    await expect(prompt).toHaveCount(0);
    expect(await flow(mock)).toEqual([]);
  });

  test("Escape cancels", async ({ page, mock }) => {
    await mock.open(snapshots.edited);
    await menu(mock, "open");
    await expect(page.getByTestId("unsaved-prompt")).toBeVisible();
    await page.keyboard.press("Escape");
    await expect(page.getByTestId("unsaved-prompt")).toHaveCount(0);
    expect(await flow(mock)).toEqual([]);
  });

  test("Don't Save discards and continues", async ({ page, mock }) => {
    await mock.open(snapshots.edited);
    await mock.reply("new_project", result({ ...snapshots.initial, revision: 2 }));
    await menu(mock, "new");
    await page.getByTestId("prompt-discard").click();
    await expect.poll(() => flow(mock)).toEqual(["new_project"]);
    await expect(page).toHaveTitle("kwikvid");
  });

  test("Save saves (as) first, then continues", async ({ page, mock }) => {
    await mock.open(snapshots.edited);
    await mock.reply("choose_save_path", TRIP);
    await mock.reply("save_project_as", result(snapshots.saved, TRIP));
    await mock.reply("choose_open_path", TRIP);
    await mock.reply("open_project", result(snapshots.opened, TRIP));
    await menu(mock, "open");
    await page.getByTestId("prompt-save").click();
    await expect
      .poll(() => flow(mock))
      .toEqual(["choose_save_path", "save_project_as", "choose_open_path", "open_project"]);
  });

  test("cancelling the Save As picker cancels the whole action", async ({ page, mock }) => {
    await mock.open(snapshots.edited);
    await mock.reply("choose_save_path", null);
    await menu(mock, "new");
    await page.getByTestId("prompt-save").click();
    await expect.poll(() => flow(mock)).toEqual(["choose_save_path"]);
    await expect(page.getByTestId("unsaved-prompt")).toHaveCount(0);
  });

  test("quit with changes asks, then quits", async ({ page, mock }) => {
    await mock.open(snapshots.edited);
    await menu(mock, "quit");
    await page.getByTestId("prompt-discard").click();
    await expect.poll(() => flow(mock)).toEqual(["quit"]);
  });

  test("quit without changes quits at once", async ({ page, mock }) => {
    await mock.open(snapshots.initial);
    await menu(mock, "quit");
    await expect.poll(() => flow(mock)).toEqual(["quit"]);
    await expect(page.getByTestId("unsaved-prompt")).toHaveCount(0);
  });

  test("a failed save keeps the app open", async ({ page, mock }) => {
    await mock.open(snapshots.edited);
    await mock.reply("choose_save_path", TRIP);
    await mock.fail("save_project_as", "The project file couldn't be read or written.");
    await menu(mock, "quit");
    await page.getByTestId("prompt-save").click();
    await expect(page.getByTestId("toast")).toBeVisible();
    expect(await flow(mock)).toEqual(["choose_save_path", "save_project_as"]);
  });

  test("a second menu action while prompting is ignored", async ({ page, mock }) => {
    await mock.open(snapshots.edited);
    await menu(mock, "new");
    await menu(mock, "open");
    await expect(page.getByTestId("unsaved-prompt")).toHaveCount(1);
    await page.getByTestId("prompt-cancel").click();
    expect(await flow(mock)).toEqual([]);
  });
});

test.describe("missing media", () => {
  test("open lists missing clips in a dismissible banner", async ({ page, mock }) => {
    await mock.open(snapshots.initial);
    await mock.reply("open_project", result(snapshots.opened, TRIP, missingClips));
    await menu(mock, "open_recent", { path: TRIP });
    const banner = page.getByTestId("missing-media");
    await expect(page.getByTestId("missing-media-summary")).toHaveText("3 clips not found");
    for (const n of [1, 2, 3]) await expect(banner).toContainText(CLIP(n));
    // The project still opened.
    await expect(page.locator("#library-summary")).toHaveText("3 items");
    await page.getByTestId("missing-media-dismiss").click();
    await expect(banner).toHaveCount(0);
  });

  test("missing music is named separately", async ({ page, mock }) => {
    await mock.open(snapshots.initial);
    const music = { kind: { type: "music", section: 1 }, project_path: "song.mp3", resolved: "/m/song.mp3" };
    await mock.reply("open_project", result(snapshots.opened, TRIP, [missingClips[0], music]));
    await menu(mock, "open_recent", { path: TRIP });
    await expect(page.getByTestId("missing-media-summary")).toHaveText(
      "1 clip and 1 music file not found",
    );
  });

  test("New clears the banner", async ({ page, mock }) => {
    await mock.open(snapshots.initial);
    await mock.reply("open_project", result(snapshots.opened, TRIP, missingClips));
    await menu(mock, "open_recent", { path: TRIP });
    await expect(page.getByTestId("missing-media")).toBeVisible();
    await mock.reply("new_project", result({ ...snapshots.initial, revision: 3 }));
    await menu(mock, "new");
    await expect(page.getByTestId("missing-media")).toHaveCount(0);
  });
});
