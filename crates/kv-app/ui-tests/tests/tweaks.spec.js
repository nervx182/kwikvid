// Manual tweaks (ui/js/tweaks.js): drag to reorder, nudge a cut by a beat,
// clip audio toggle, music volume committed on release.
//
// timeline-edit (see crates/kv-app/tests/ui_fixtures.rs): 8 s span, four
// segments at 0-2 s (clip 1), 1.5-4 s (clip 2), 4-6 s (clip 3) and 6-8 s
// (clip 1); cuts 1-3 at 1.5, 4 and 6 s. The tweaks-* fixtures are one
// command applied to it.

import { test, expect, snapshots } from "../fixtures.js";

const SEC = 1_000_000;
const edit = snapshots["timeline-edit"];
const beats = snapshots["beats-song"];
const section = edit.project.sections[0].id;

/** Page coordinates of timeline time `t` on the segment track. */
async function trackPoint(page, t) {
  const content = await page.getByTestId("timeline-content").boundingBox();
  const track = await page.getByTestId("timeline-track").boundingBox();
  return { x: content.x + (t / (8 * SEC)) * content.width, y: track.y + track.height / 2 };
}

async function press(page, t) {
  const p = await trackPoint(page, t);
  await page.mouse.move(p.x, p.y);
  await page.mouse.down();
}

async function moveTo(page, t) {
  const p = await trackPoint(page, t);
  await page.mouse.move(p.x, p.y, { steps: 6 });
}

const cut = (page, i) => page.locator(`[data-testid="timeline-cut"][data-cut="${i}"]`);

test("dragging a segment between others sends MoveSegment and shows a drop indicator", async ({
  page,
  mock,
}) => {
  await mock.open(edit, { beats });
  await expect(page.locator(".tl-seg")).toHaveCount(4);
  await mock.respond("apply", snapshots["tweaks-moved"]);
  await press(page, 1 * SEC); // segment 0
  await moveTo(page, 5.5 * SEC); // past the middle of segment 2
  const drop = page.getByTestId("timeline-drop");
  await expect(drop).toBeVisible();
  // The gap before segment 3 (6 s, three quarters of the span).
  await expect(drop).toHaveAttribute("data-gap", "3");
  const content = await page.getByTestId("timeline-content").boundingBox();
  const d = await drop.boundingBox();
  expect(Math.abs((d.x + d.width / 2 - content.x) / content.width - 0.75)).toBeLessThan(0.01);
  await expect(page.locator(".tl-seg").nth(0)).toHaveClass(/tl-dragging/);
  // Nothing is sent before the release.
  expect(await mock.commands("apply")).toEqual([]);
  await page.mouse.up();
  await expect.poll(() => mock.commands("apply")).toHaveLength(1);
  const [{ args }] = await mock.commands("apply");
  expect(args.cmd).toEqual({ type: "move_segment", section, from: 0, to: 2 });
  await expect(drop).toBeHidden();
  // The answer renders: clip 2 is first now.
  await expect(page.locator(".tl-seg").nth(0)).toHaveAttribute("title", "GX010002.MP4");
  await expect(page.locator(".tl-seg").nth(2)).toHaveAttribute("title", "GX010001.MP4");
});

test("Esc cancels a drag, and a drop in place sends nothing", async ({ page, mock }) => {
  await mock.open(edit, { beats });
  await press(page, 1 * SEC);
  await moveTo(page, 7.5 * SEC);
  await expect(page.getByTestId("timeline-drop")).toBeVisible();
  await page.keyboard.press("Escape");
  await expect(page.getByTestId("timeline-drop")).toBeHidden();
  await expect(page.locator(".tl-dragging")).toHaveCount(0);
  await page.mouse.up();

  // Dragging segment 0 a little stays in its own gap: no move.
  await press(page, 1 * SEC);
  await moveTo(page, 1.4 * SEC);
  await expect(page.getByTestId("timeline-drop")).toBeHidden();
  await page.mouse.up();
  // A click without a drag does nothing either.
  await page.mouse.click((await trackPoint(page, 5 * SEC)).x, (await trackPoint(page, 5 * SEC)).y);
  expect(await mock.commands("apply")).toEqual([]);
});

test("selecting a cut shows the nudge buttons, which send nudge_cut", async ({ page, mock }) => {
  await mock.open(edit, { beats });
  await expect(page.getByTestId("timeline-cut")).toHaveCount(3);
  await expect(page.getByTestId("tweaks-nudge")).toBeHidden();
  await expect(cut(page, 2)).toHaveAttribute("data-t", String(4 * SEC));
  await cut(page, 2).click();
  await expect(cut(page, 2)).toHaveAttribute("aria-pressed", "true");
  await expect(page.getByTestId("tweaks-nudge")).toBeVisible();
  await expect(page.getByTestId("tweaks-cut-label")).toHaveText("Cut 2");

  await mock.respond("nudge_cut", snapshots["tweaks-nudged"]);
  await page.getByTestId("tweaks-nudge-later").click();
  await expect.poll(() => mock.commands("nudge_cut")).toHaveLength(1);
  expect((await mock.commands("nudge_cut"))[0].args).toEqual({ section, cut: 2, beats: 1 });
  // The answer moves the cut to 4.5 s; it stays selected.
  await expect(cut(page, 2)).toHaveAttribute("data-t", String(4.5 * SEC));
  await expect(cut(page, 2)).toHaveAttribute("aria-pressed", "true");

  await page.getByTestId("tweaks-nudge-earlier").click();
  await expect.poll(() => mock.commands("nudge_cut")).toHaveLength(2);
  expect((await mock.commands("nudge_cut"))[1].args).toEqual({ section, cut: 2, beats: -1 });
  // Nudges are not plain commands and never ask for a rebuild.
  expect(await mock.commands("apply")).toEqual([]);
  expect(await mock.commands("reroll")).toEqual([]);
});

test("keys , and . nudge the selected cut; Esc deselects", async ({ page, mock }) => {
  await mock.open(edit, { beats });
  // Without a selected cut the keys do nothing.
  await page.keyboard.press(".");
  await cut(page, 1).click();
  await page.keyboard.press(",");
  await page.keyboard.press(".");
  await expect.poll(() => mock.commands("nudge_cut")).toHaveLength(2);
  const calls = await mock.commands("nudge_cut");
  expect(calls.map((c) => c.args)).toEqual([
    { section, cut: 1, beats: -1 },
    { section, cut: 1, beats: 1 },
  ]);
  await page.keyboard.press("Escape");
  await expect(page.getByTestId("tweaks-nudge")).toBeHidden();
  await expect(cut(page, 1)).toHaveAttribute("aria-pressed", "false");
  await page.keyboard.press(".");
  expect(await mock.commands("nudge_cut")).toHaveLength(2);
});

test("a rejected nudge shows why", async ({ page, mock }) => {
  await mock.open(edit, { beats });
  await mock.fail("nudge_cut", "Moving that cut would make a clip too short.");
  await cut(page, 3).click();
  await page.getByTestId("tweaks-nudge-earlier").click();
  await expect(page.getByTestId("toasts")).toContainText("Moving that cut would make a clip too short.");
});

test("clip audio toggles from the segment menu and shows a speaker icon", async ({ page, mock }) => {
  await mock.open(edit, { beats });
  await expect(page.getByTestId("timeline-clip-audio")).toHaveCount(0);
  const p = await trackPoint(page, 7 * SEC); // segment 3, clip 1
  await page.mouse.click(p.x, p.y, { button: "right" });
  const item = page.getByTestId("tweaks-clip-audio");
  await expect(item).toHaveAttribute("aria-checked", "false");
  await mock.respond("apply", snapshots["tweaks-clip-audio"]);
  await item.click();
  await expect(page.getByTestId("timeline-menu")).toHaveCount(0);
  await expect.poll(() => mock.commands("apply")).toHaveLength(1);
  expect((await mock.commands("apply"))[0].args.cmd).toEqual({
    type: "set_clip_audio",
    media_id: 1,
    enabled: true,
  });
  // Both segments of clip 1 show the speaker.
  await expect(page.getByTestId("timeline-clip-audio")).toHaveCount(2);
  await expect(page.locator(".tl-seg").nth(0).getByTestId("timeline-clip-audio")).toBeVisible();
  await expect(page.locator(".tl-seg").nth(3)).toHaveAttribute("data-clip-audio", "true");

  // Toggle back off; Esc closes the menu without sending anything.
  await page.mouse.click(p.x, p.y, { button: "right" });
  await expect(item).toHaveAttribute("aria-checked", "true");
  await page.keyboard.press("Escape");
  await expect(page.getByTestId("timeline-menu")).toHaveCount(0);
  await page.mouse.click(p.x, p.y, { button: "right" });
  await item.click();
  await expect.poll(() => mock.commands("apply")).toHaveLength(2);
  expect((await mock.commands("apply"))[1].args.cmd).toEqual({
    type: "set_clip_audio",
    media_id: 1,
    enabled: false,
  });
});

test("music volume is sent once, when the slider is released", async ({ page, mock }) => {
  await mock.open(edit, { beats });
  const slider = page.getByTestId("tweaks-volume");
  await expect(slider).toHaveValue("100");
  await expect(page.getByTestId("tweaks-volume-value")).toHaveText("100%");
  const box = await slider.boundingBox();
  const y = box.y + box.height / 2;
  await mock.respond("apply", snapshots["tweaks-volume"]);
  await page.mouse.move(box.x + box.width - 2, y);
  await page.mouse.down();
  await page.mouse.move(box.x + box.width * 0.6, y, { steps: 4 });
  await page.mouse.move(box.x + box.width * 0.4, y, { steps: 4 });
  // The readout follows the drag, but nothing is sent yet.
  await expect(page.getByTestId("tweaks-volume-value")).not.toHaveText("100%");
  expect(await mock.commands("apply")).toEqual([]);
  await page.mouse.up();
  await expect.poll(() => mock.commands("apply")).toHaveLength(1);
  const { cmd } = (await mock.commands("apply"))[0].args;
  expect(cmd.type).toBe("set_music_volume");
  expect(cmd.percent).toBeGreaterThan(20);
  expect(cmd.percent).toBeLessThan(60);
  // The answer's volume (40%) is shown.
  await expect(slider).toHaveValue("40");
  await expect(page.getByTestId("tweaks-volume-value")).toHaveText("40%");

  // Keyboard: one step, one command.
  await slider.focus();
  await page.keyboard.press("ArrowRight");
  await expect.poll(() => mock.commands("apply")).toHaveLength(2);
  expect((await mock.commands("apply"))[1].args.cmd).toEqual({ type: "set_music_volume", percent: 41 });
});

test("the hint says what replaces manual tweaks", async ({ page, mock }) => {
  await mock.open(snapshots.initial);
  await expect(page.getByTestId("tweaks-hint")).toBeHidden();
  await expect(page.getByTestId("tweaks-volume")).toBeDisabled();
  await mock.state(edit);
  await expect(page.getByTestId("tweaks-hint")).toBeVisible();
  await expect(page.getByTestId("tweaks-hint")).toContainText("replaces these tweaks");
  await expect(page.getByTestId("tweaks-volume")).toBeEnabled();
});
