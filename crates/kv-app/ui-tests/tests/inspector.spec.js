// Clip inspector: opens for the selected library item; scrub head and
// nearest frame, HiLight ticks, I/O marks, Always include / Exclude, pin and
// exclude overlays with remove buttons, rejections; "Exclude clip" in the
// library context menu.
//
// Fixtures (see crates/kv-app/tests/ui_fixtures.rs), all on top of library
// (clip 1 GX010001.MP4 75 s with HiLights at 15 s and 60 s, clip 2 10 s,
// clip 3 40 s with a HiLight at 30 s):
// inspector-pinned: clip 1 10-15 s pinned.
// inspector-constraints: that pin, clip 1 40-50 s excluded, clip 2 excluded.
// inspector-pins-too-long: clip 1 0-20 s pinned over a 15 s target
// (regen_error).

import { test, expect, snapshots } from "../fixtures.js";

const library = snapshots.library;
const pinned = snapshots["inspector-pinned"];
const constraints = snapshots["inspector-constraints"];
const tooLong = snapshots["inspector-pins-too-long"];
const S = 1_000_000;

const row = (page, id) => page.locator(`[data-testid=library-item][data-media-id="${id}"]`);
const strip = (page) => page.getByTestId("inspector-filmstrip");
const head = (page) => page.getByTestId("inspector-head");

/** Select clip `id` in the library; the inspector opens. */
async function select(page, id) {
  await row(page, id).locator(".lib-name").click();
  await expect(page.getByTestId("clip-inspector")).toBeVisible();
}

/** Move the scrub head to `seconds` (whole seconds) with the keyboard. */
async function scrub(page, seconds) {
  await strip(page).focus();
  await page.keyboard.press("Home");
  for (let i = 0; i < seconds; i++) await page.keyboard.press("ArrowRight");
  await expect(head(page)).toHaveAttribute("data-time", String(seconds * S));
}

const apply = (cmd) => ({ command: "apply", args: { cmd } });
const range = (media_id, start, end) => ({ media_id, start, end });

/** Left and width of an element as fractions of the inspector filmstrip. */
async function span(page, locator) {
  const s = await strip(page).boundingBox();
  const b = await locator.boundingBox();
  return [(b.x - s.x) / s.width, b.width / s.width];
}

function near(got, want, tol = 0.01) {
  got.forEach((g, i) => expect(Math.abs(g - want[i]), `value ${i}: ${g} vs ${want[i]}`).toBeLessThan(tol));
}

test("selecting a library item opens the inspector with filmstrip, head and HiLight ticks", async ({ page, mock }) => {
  await mock.open(library);
  await expect(page.getByTestId("clip-inspector")).toBeHidden();
  await select(page, 1);
  await expect(page.getByTestId("inspector-name")).toHaveText("GX010001.MP4");
  await expect(page.locator(".ins-cell")).toHaveCount(12);
  await expect(page.locator(".ins-cell").nth(3)).toHaveAttribute("data-thumb", "kvthumb://1/3");
  await expect(page.getByTestId("inspector-time")).toHaveText("0:00.0 / 1:15.0");
  await expect(page.getByTestId("inspector-frame")).toHaveAttribute("src", "kvthumb://1/0");
  const ticks = page.getByTestId("inspector-hilight");
  await expect(ticks).toHaveCount(2);
  await expect(ticks.nth(0)).toHaveAttribute("data-time", String(15 * S));
  const s = await strip(page).boundingBox();
  const b = await ticks.nth(1).boundingBox();
  near([(b.x + b.width / 2 - s.x) / s.width], [60 / 75]);
  await expect(page.getByTestId("inspector-pin")).toHaveCount(0);

  // Another clip replaces it; deselecting (removing the clip) closes it.
  await select(page, 3);
  await expect(page.getByTestId("inspector-name")).toHaveText("GX010042.MP4");
  await expect(page.getByTestId("inspector-hilight")).toHaveCount(1);
});

test("scrubbing moves the head and shows the nearest filmstrip frame at once", async ({ page, mock }) => {
  await mock.open(library);
  await select(page, 1);
  const s = await strip(page).boundingBox();
  const y = s.y + s.height / 2;
  // Aim at cell centres ((i + 0.5) / 12), never at a cell boundary: WebKit
  // delivers whole-pixel mouse coordinates, so a click exactly on a boundary
  // can land in either cell.
  const centre = (cell) => s.x + (s.width * (cell + 0.5)) / 12;
  await page.mouse.click(centre(6), y);
  const t = Number(await head(page).getAttribute("data-time"));
  expect(Math.abs(t - (6.5 / 12) * 75 * S)).toBeLessThan(0.01 * 75 * S);
  await expect(page.getByTestId("inspector-frame")).toHaveAttribute("data-cell", "6");
  await expect(page.getByTestId("inspector-frame")).toHaveAttribute("src", "kvthumb://1/6");
  await expect(page.getByTestId("inspector-time")).toContainText(/^0:40\.\d \/ 1:15\.0$/);

  // Dragging follows the pointer.
  await page.mouse.move(centre(1), y);
  await page.mouse.down();
  await page.mouse.move(centre(6), y, { steps: 3 });
  await page.mouse.move(centre(10), y, { steps: 3 });
  await page.mouse.up();
  await expect(page.getByTestId("inspector-frame")).toHaveAttribute("data-cell", "10");

  // Keyboard: End, Shift+Left (0.1 s), Home.
  await strip(page).focus();
  await page.keyboard.press("End");
  await expect(head(page)).toHaveAttribute("data-time", String(75 * S));
  await expect(page.getByTestId("inspector-frame")).toHaveAttribute("data-cell", "11");
  await page.keyboard.press("Shift+ArrowLeft");
  await expect(head(page)).toHaveAttribute("data-time", String(74.9 * S));
  await expect(page.getByTestId("inspector-time")).toHaveText("1:14.9 / 1:15.0");
  // No commands were sent by scrubbing.
  expect(await mock.commands("apply")).toEqual([]);
});

test("I / O + Always include sends the exact PinRange and shows the pin", async ({ page, mock }) => {
  await mock.open(library);
  await select(page, 1);
  await scrub(page, 10);
  await page.keyboard.press("i");
  await expect(page.getByTestId("inspector-in")).toHaveAttribute("data-time", String(10 * S));
  await scrub(page, 15);
  await page.keyboard.press("o");
  await expect(page.getByTestId("inspector-out")).toHaveAttribute("data-time", String(15 * S));
  await expect(page.getByTestId("inspector-range")).toHaveText("Range 0:10.0–0:15.0");

  await mock.respond("apply", pinned);
  await page.getByTestId("inspector-include").click();
  expect(await mock.commands("apply")).toEqual([
    apply({ type: "pin_range", section: 1, range: range(1, 10 * S, 15 * S) }),
  ]);
  const pin = page.getByTestId("inspector-pin");
  await expect(pin).toHaveCount(1);
  await expect(pin).toHaveAttribute("data-start", String(10 * S));
  await expect(pin).toHaveAttribute("data-end", String(15 * S));
  near(await span(page, pin), [10 / 75, 5 / 75]);
  // A clean success clears the marks.
  await expect(page.getByTestId("inspector-in")).toBeHidden();
  await expect(page.getByTestId("inspector-out")).toBeHidden();
  await expect(page.getByTestId("inspector-error")).toBeHidden();
});

test("the In / Out buttons mark too, and Exclude sends ExcludeRange", async ({ page, mock }) => {
  await mock.open(library);
  await select(page, 3);
  await scrub(page, 5);
  await page.getByTestId("inspector-mark-in").click();
  await scrub(page, 12);
  await page.getByTestId("inspector-mark-out").click();
  await page.getByTestId("inspector-exclude").click();
  expect(await mock.commands("apply")).toEqual([
    apply({ type: "exclude_range", section: 1, range: range(3, 5 * S, 12 * S) }),
  ]);
});

test("a single mark makes a 2 s range centred on it, clamped to the clip", async ({ page, mock }) => {
  await mock.open(library);
  await select(page, 1);
  // In at 30 s only: 29-31 s.
  await scrub(page, 30);
  await page.keyboard.press("i");
  await page.getByTestId("inspector-include").click();
  // Out at the very end only: 73-75 s.
  await page.getByTestId("inspector-clear").click();
  await strip(page).focus();
  await page.keyboard.press("End");
  await page.keyboard.press("o");
  await expect(page.getByTestId("inspector-range")).toHaveText("Range 1:13.0–1:15.0");
  await page.getByTestId("inspector-exclude").click();
  // In at 0.4 s: shifted to 0-2 s.
  await page.getByTestId("inspector-clear").click();
  await strip(page).focus();
  await page.keyboard.press("Home");
  for (let i = 0; i < 4; i++) await page.keyboard.press("Shift+ArrowRight");
  await page.keyboard.press("i");
  await page.getByTestId("inspector-include").click();
  expect(await mock.commands("apply")).toEqual([
    apply({ type: "pin_range", section: 1, range: range(1, 29 * S, 31 * S) }),
    apply({ type: "exclude_range", section: 1, range: range(1, 73 * S, 75 * S) }),
    apply({ type: "pin_range", section: 1, range: range(1, 0, 2 * S) }),
  ]);
});

test("clicking a HiLight tick pins 2 s around it", async ({ page, mock }) => {
  await mock.open(library);
  await select(page, 1);
  await page.getByTestId("inspector-hilight").nth(1).click();
  expect(await mock.commands("apply")).toEqual([
    apply({ type: "pin_range", section: 1, range: range(1, 59 * S, 61 * S) }),
  ]);
  await expect(head(page)).toHaveAttribute("data-time", String(60 * S));
});

test("Exclude clip in the library menu excludes 0..duration and dims the row", async ({ page, mock }) => {
  await mock.open(library);
  await expect(row(page, 2)).toHaveAttribute("data-excluded", "false");
  await row(page, 2).click({ button: "right" });
  await page.getByTestId("library-exclude-clip").click();
  // Answer with a state where clip 2 is excluded (plus clip 1's constraints).
  expect(await mock.commands("apply")).toEqual([
    apply({ type: "exclude_range", section: 1, range: range(2, 0, 10 * S) }),
  ]);
  await expect(page.getByTestId("library-menu")).toHaveCount(0);
  await mock.state(constraints);
  await expect(row(page, 2)).toHaveAttribute("data-excluded", "true");
  await expect(row(page, 1)).toHaveAttribute("data-excluded", "false");
  expect(Number(await row(page, 2).evaluate((e) => getComputedStyle(e).opacity))).toBeLessThan(1);
  // The inspector shows the whole clip hatched.
  await select(page, 2);
  near(await span(page, page.getByTestId("inspector-excluded")), [0, 1]);

  // The menu now offers Include clip, which sends the exact exclude back.
  await row(page, 2).click({ button: "right" });
  await expect(page.getByTestId("library-exclude-clip")).toHaveCount(0);
  await page.getByTestId("library-include-clip").click();
  expect((await mock.commands("apply"))[1]).toEqual(
    apply({ type: "unexclude_range", section: 1, range: range(2, 0, 10 * S) }),
  );
});

test("overlays show pins and excludes; × removes them with the exact range", async ({ page, mock }) => {
  await mock.open(constraints);
  await select(page, 1);
  const pin = page.locator(".ins-overlay.ins-pin");
  const ex = page.locator(".ins-overlay.ins-exclude");
  await expect(pin).toHaveCount(1);
  await expect(ex).toHaveCount(1);
  near(await span(page, pin), [10 / 75, 5 / 75]);
  near(await span(page, ex), [40 / 75, 10 / 75]);
  const bg = await ex.evaluate((e) => getComputedStyle(e).backgroundImage);
  expect(bg).toContain("repeating-linear-gradient");

  // constraints without the pin, then without clip 1's exclude too.
  const unpinned = structuredClone(constraints);
  unpinned.revision += 1;
  unpinned.project.sections[0].pins = [];
  const unexcluded = structuredClone(unpinned);
  unexcluded.revision += 1;
  unexcluded.project.sections[0].excludes.shift();
  await mock.respond("apply", unpinned);
  await pin.getByTestId("inspector-remove").click();
  await expect(pin).toHaveCount(0);
  await mock.respond("apply", unexcluded);
  await ex.getByTestId("inspector-remove").click();
  expect(await mock.commands("apply")).toEqual([
    apply({ type: "unpin_range", section: 1, range: range(1, 10 * S, 15 * S) }),
    apply({ type: "unexclude_range", section: 1, range: range(1, 40 * S, 50 * S) }),
  ]);
  await expect(page.locator(".ins-overlay")).toHaveCount(0);
});

test("a rejected range shows the session's message and keeps the marks", async ({ page, mock }) => {
  await mock.open(constraints);
  await select(page, 1);
  await scrub(page, 45);
  await page.keyboard.press("i");
  await scrub(page, 48);
  await page.keyboard.press("o");
  const message = "That overlaps a moment already forced in or excluded.";
  await mock.fail("apply", message);
  await page.getByTestId("inspector-include").click();
  await expect(page.getByTestId("inspector-error")).toHaveText(message);
  await expect(page.getByTestId("inspector-in")).toHaveAttribute("data-time", String(45 * S));
  await expect(page.getByTestId("inspector-out")).toHaveAttribute("data-time", String(48 * S));
  await expect(page.locator(".ins-overlay.ins-pin")).toHaveCount(1);
  // Marking again clears the message.
  await page.keyboard.press("o");
  await expect(page.getByTestId("inspector-error")).toBeHidden();
});

test("a pin longer than the target shows regen_error and keeps the marks", async ({ page, mock }) => {
  await mock.open(library);
  await select(page, 1);
  await scrub(page, 0);
  await page.keyboard.press("i");
  await scrub(page, 20);
  await page.keyboard.press("o");
  await mock.respond("apply", tooLong);
  await page.getByTestId("inspector-include").click();
  expect(await mock.commands("apply")).toEqual([
    apply({ type: "pin_range", section: 1, range: range(1, 0, 20 * S) }),
  ]);
  await expect(page.getByTestId("inspector-error")).toHaveText(tooLong.regen_error);
  await expect(page.getByTestId("inspector-in")).toHaveAttribute("data-time", "0");
  await expect(page.getByTestId("inspector-out")).toHaveAttribute("data-time", String(20 * S));
});

test("I and O do nothing while no clip is selected or while typing", async ({ page, mock }) => {
  await mock.open(library);
  await page.keyboard.press("i");
  await expect(page.getByTestId("inspector-in")).toBeHidden();
  await select(page, 1);
  await page.keyboard.press("Meta+i");
  await page.keyboard.press("Control+o");
  await expect(page.getByTestId("inspector-in")).toBeHidden();
  await expect(page.getByTestId("inspector-out")).toBeHidden();
  expect(await mock.commands("apply")).toEqual([]);
});
