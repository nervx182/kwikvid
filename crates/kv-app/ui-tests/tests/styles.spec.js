// Styles: the preset picker (set_style), the title card field and duration
// (set_title_card on blur/Enter, null when cleared, 80 characters at most)
// and the title preview over the viewer.

import { test, expect, snapshots } from "../fixtures.js";

const SEC = 1_000_000;
const ready = snapshots["autoedit-ready"];
const smooth = snapshots["styles-smooth"];
const titled = snapshots["styles-titled"];

/** `s` at a newer revision (so the UI does not drop it as stale). */
function newer(s, revision) {
  return { ...structuredClone(s), revision };
}

const applied = async (mock) => (await mock.commands("apply")).map((c) => c.args.cmd);

test("the fixtures say what the tests assume", () => {
  expect(ready.project.sections[0].style).toBe("cut");
  expect(ready.project.sections[0].title).toBeNull();
  expect(smooth.project.sections[0].style).toBe("smooth");
  expect(smooth.project.sections[0].title).toBeNull();
  expect(titled.project.sections[0].title).toEqual({ text: "Lake day", duration: 3 * SEC });
});

test("three preset cards with a name, description and icon; the current one is checked", async ({ page, mock }) => {
  await mock.open(ready);
  const cards = page.getByTestId("style-presets").getByRole("radio");
  await expect(cards).toHaveCount(3);
  for (const [id, name] of [
    ["cut", "Cut"],
    ["smooth", "Smooth"],
    ["action", "Action"],
  ]) {
    const card = page.getByTestId(`style-${id}`);
    await expect(card.locator(".sty-name")).toHaveText(name);
    await expect(card.locator(".sty-desc")).not.toBeEmpty();
    await expect(card.locator("svg")).toHaveCount(1);
    await expect(card).toHaveAttribute("aria-checked", id === "cut" ? "true" : "false");
  }
});

for (const id of ["smooth", "action"]) {
  test(`choosing ${id} sends set_style`, async ({ page, mock }) => {
    await mock.open(ready);
    await page.getByTestId(`style-${id}`).click();
    await expect.poll(() => applied(mock)).toEqual([{ type: "set_style", section: 1, style: id }]);
  });
}

test("the answer's style is shown; clicking the current style sends nothing", async ({ page, mock }) => {
  await mock.open(ready);
  await mock.respond("apply", smooth);
  await page.getByTestId("style-smooth").click();
  await expect(page.getByTestId("style-smooth")).toHaveAttribute("aria-checked", "true");
  await expect(page.getByTestId("style-cut")).toHaveAttribute("aria-checked", "false");
  await page.getByTestId("style-smooth").click();
  await page.getByTestId("style-cut").click();
  expect(await applied(mock)).toEqual([
    { type: "set_style", section: 1, style: "smooth" },
    { type: "set_style", section: 1, style: "cut" },
  ]);
});

test("a rejected style shows a toast and keeps the old selection", async ({ page, mock }) => {
  await mock.open(ready);
  await mock.fail("apply", "No such section.");
  await page.getByTestId("style-action").click();
  await expect(page.getByTestId("toasts")).toContainText("No such section.");
  await expect(page.getByTestId("style-cut")).toHaveAttribute("aria-checked", "true");
});

test("Enter commits the title with the chosen duration", async ({ page, mock }) => {
  await mock.open(smooth);
  const input = page.getByTestId("style-title");
  await page.getByTestId("style-title-duration").selectOption(String(4 * SEC));
  expect(await applied(mock)).toEqual([]); // no text yet: nothing to commit
  await input.fill("  Lake day  ");
  expect(await applied(mock)).toEqual([]); // not committed while typing
  await input.press("Enter");
  await expect.poll(() => applied(mock)).toEqual([
    { type: "set_title_card", section: 1, title: { text: "Lake day", duration: 4 * SEC } },
  ]);
});

test("blur commits the title; an unchanged title sends nothing", async ({ page, mock }) => {
  await mock.open(smooth);
  const input = page.getByTestId("style-title");
  await input.fill("Lake day");
  await mock.respond("apply", titled);
  await input.blur();
  await expect.poll(() => applied(mock)).toEqual([
    { type: "set_title_card", section: 1, title: { text: "Lake day", duration: 3 * SEC } },
  ]);
  await input.focus();
  await input.press("Enter");
  await input.blur();
  expect(await applied(mock)).toHaveLength(1);
});

test("the project's title is shown and clearing it sends title: null", async ({ page, mock }) => {
  await mock.open(titled);
  const input = page.getByTestId("style-title");
  await expect(input).toHaveValue("Lake day");
  await expect(page.getByTestId("style-title-duration")).toHaveValue(String(3 * SEC));
  await input.fill("");
  await input.press("Enter");
  await expect.poll(() => applied(mock)).toEqual([{ type: "set_title_card", section: 1, title: null }]);
});

test("whitespace only counts as clearing", async ({ page, mock }) => {
  await mock.open(titled);
  await page.getByTestId("style-title").fill("   ");
  await page.getByTestId("style-title").blur();
  await expect.poll(() => applied(mock)).toEqual([{ type: "set_title_card", section: 1, title: null }]);
});

test("changing the duration of a title commits it", async ({ page, mock }) => {
  await mock.open(titled);
  await page.getByTestId("style-title-duration").selectOption(String(2 * SEC));
  await expect.poll(() => applied(mock)).toEqual([
    { type: "set_title_card", section: 1, title: { text: "Lake day", duration: 2 * SEC } },
  ]);
});

test("Escape reverts the field without sending", async ({ page, mock }) => {
  await mock.open(titled);
  const input = page.getByTestId("style-title");
  await input.fill("Something else");
  await input.press("Escape");
  await expect(input).toHaveValue("Lake day");
  await expect(input).not.toBeFocused();
  expect(await applied(mock)).toEqual([]);
});

test("the title is at most 80 characters", async ({ page, mock }) => {
  await mock.open(smooth);
  const input = page.getByTestId("style-title");
  await expect(input).toHaveAttribute("maxlength", "80");
  await input.pressSequentially("x".repeat(90));
  await expect(input).toHaveValue("x".repeat(80));
  await input.press("Enter");
  await expect.poll(() => applied(mock)).toEqual([
    { type: "set_title_card", section: 1, title: { text: "x".repeat(80), duration: 3 * SEC } },
  ]);
});

test("a rejected title shows a toast and the field goes back to the project's title", async ({ page, mock }) => {
  await mock.open(titled);
  const input = page.getByTestId("style-title");
  await input.fill("New");
  await mock.fail("apply", "The title is too long.");
  await input.blur();
  await expect(page.getByTestId("toasts")).toContainText("The title is too long.");
  await expect(input).toHaveValue("Lake day");
});

test("a new state does not overwrite what is being typed", async ({ page, mock }) => {
  await mock.open(smooth);
  const input = page.getByTestId("style-title");
  await input.fill("Half typ");
  await mock.state(newer(titled, titled.revision + 1));
  await expect(input).toHaveValue("Half typ");
  await expect(page.getByTestId("title-overlay")).toHaveText("Lake day");
});

test("typing in the title does not trigger shortcuts", async ({ page, mock }) => {
  await mock.open(smooth);
  await page.getByTestId("style-title").pressSequentially("r i o");
  await expect(page.getByTestId("style-title")).toHaveValue("r i o");
  expect(await mock.commands("reroll")).toEqual([]);
  expect(await mock.commands("play")).toEqual([]);
});

test("the title shows over the viewer only during its first seconds", async ({ page, mock }) => {
  await mock.open(titled);
  const overlay = page.getByTestId("title-overlay");
  await expect(overlay).toBeVisible();
  await expect(overlay).toHaveText("Lake day");
  // The overlay sits inside the viewer region.
  const v = await page.getByTestId("viewer").boundingBox();
  const o = await overlay.boundingBox();
  expect(o.x).toBeGreaterThanOrEqual(v.x);
  expect(o.y).toBeGreaterThanOrEqual(v.y);
  expect(o.x + o.width).toBeLessThanOrEqual(v.x + v.width);
  expect(o.y + o.height).toBeLessThanOrEqual(v.y + v.height);

  await mock.event("kv://position", { position: 2_500_000, playing: true, frame_us: 33_333 });
  await expect(page.getByTestId("timeline-playhead")).toHaveAttribute("data-position", "2500000");
  await expect(overlay).toBeVisible();
  await mock.event("kv://position", { position: 3_000_000, playing: true, frame_us: 33_333 });
  await expect(overlay).toBeHidden();
  await mock.event("kv://position", { position: 0, playing: false, frame_us: 33_333 });
  await expect(overlay).toBeVisible();
});

test("no title, no overlay; clearing the title hides it", async ({ page, mock }) => {
  await mock.open(smooth);
  await expect(page.getByTestId("title-overlay")).toBeHidden();
  await mock.state(newer(titled, titled.revision + 1));
  await expect(page.getByTestId("title-overlay")).toBeVisible();
  await mock.state(newer(smooth, titled.revision + 2));
  await expect(page.getByTestId("title-overlay")).toBeHidden();
});

test("a new project shows Cut and an empty title", async ({ page, mock }) => {
  await mock.open(snapshots.initial);
  await expect(page.getByTestId("style-cut")).toHaveAttribute("aria-checked", "true");
  await expect(page.getByTestId("style-title")).toHaveValue("");
  await expect(page.getByTestId("title-overlay")).toBeHidden();
});
