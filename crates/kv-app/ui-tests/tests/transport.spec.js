// Transport: play/pause, keyboard stepping, scrubbing on the ruler, the
// playhead following kv://position, the m:ss.ff readout and the debug
// overlay. timeline-edit is an 8 s edit; beats-song has a beat every 0.5 s.
// The mock's preview runs at 30 fps (frame_us 33333.33).

import { test, expect, snapshots } from "../fixtures.js";

const SEC = 1_000_000;
const FRAME = SEC / 30;
const edit = snapshots["timeline-edit"];
const beats = snapshots["beats-song"];

async function seeks(mock) {
  return (await mock.commands("seek")).map((c) => c.args.position);
}

/** Press `key` and wait until the UI has sent `n` seeks in all. */
async function pressAndSeek(page, mock, key, n) {
  await page.keyboard.press(key);
  await expect.poll(async () => (await seeks(mock)).length).toBe(n);
  return (await seeks(mock))[n - 1];
}

test("space toggles play and pause", async ({ page, mock }) => {
  await mock.open(edit);
  const btn = page.getByTestId("transport-play");
  await expect(btn).toHaveAttribute("data-playing", "false");
  await page.keyboard.press("Space");
  await expect(btn).toHaveAttribute("data-playing", "true");
  await expect(btn).toHaveAttribute("aria-label", "Pause");
  await page.keyboard.press("Space");
  await expect(btn).toHaveAttribute("data-playing", "false");
  expect((await mock.commands()).filter((c) => ["play", "pause"].includes(c.command))).toEqual([
    { command: "play" },
    { command: "pause" },
  ]);
  // The button does the same, and Space on the focused button toggles once.
  await btn.click();
  await expect(btn).toHaveAttribute("data-playing", "true");
  await page.keyboard.press("Space");
  await expect(btn).toHaveAttribute("data-playing", "false");
  expect(await mock.commands("play")).toHaveLength(2);
  expect(await mock.commands("pause")).toHaveLength(2);
});

test("arrows step frames, shift+arrows step beats, home and end jump", async ({ page, mock }) => {
  await mock.open(edit, { beats });
  await expect(page.locator(".tl-beat")).toHaveCount(16);
  let n = 0;
  expect(await pressAndSeek(page, mock, "ArrowRight", ++n)).toBe(Math.round(FRAME));
  expect(await pressAndSeek(page, mock, "ArrowRight", ++n)).toBe(Math.round(2 * FRAME));
  expect(await pressAndSeek(page, mock, "ArrowLeft", ++n)).toBe(Math.round(FRAME));
  expect(await pressAndSeek(page, mock, "Shift+ArrowRight", ++n)).toBe(SEC / 2);
  expect(await pressAndSeek(page, mock, "Shift+ArrowRight", ++n)).toBe(SEC);
  expect(await pressAndSeek(page, mock, "Shift+ArrowLeft", ++n)).toBe(SEC / 2);
  // From a beat, one frame on is frame 16.
  expect(await pressAndSeek(page, mock, "ArrowRight", ++n)).toBe(Math.round(16 * FRAME));
  expect(await pressAndSeek(page, mock, "End", ++n)).toBe(8 * SEC);
  await expect(page.getByTestId("transport-time")).toHaveText("0:08.00");
  expect(await pressAndSeek(page, mock, "Home", ++n)).toBe(0);
  await expect(page.getByTestId("transport-time")).toHaveText("0:00.00");
  // Nothing before the start.
  expect(await pressAndSeek(page, mock, "ArrowLeft", ++n)).toBe(0);
});

test("stepping a frame while playing pauses first", async ({ page, mock }) => {
  await mock.open(edit);
  await page.keyboard.press("Space");
  await expect(page.getByTestId("transport-play")).toHaveAttribute("data-playing", "true");
  await page.keyboard.press("ArrowRight");
  await expect.poll(() => mock.commands("seek")).toHaveLength(1);
  const names = (await mock.commands()).map((c) => c.command);
  expect(names.filter((c) => ["play", "pause", "seek"].includes(c))).toEqual([
    "play",
    "pause",
    "seek",
  ]);
  await expect(page.getByTestId("transport-play")).toHaveAttribute("data-playing", "false");
});

test("beat stepping without beats does nothing", async ({ page, mock }) => {
  await mock.open(edit);
  await page.keyboard.press("Shift+ArrowRight");
  await page.keyboard.press("ArrowRight");
  await expect.poll(() => mock.commands("seek")).toHaveLength(1);
  expect(await seeks(mock)).toEqual([Math.round(FRAME)]);
});

test("the playhead and readout follow position events", async ({ page, mock }) => {
  await mock.open(edit);
  await expect(page.getByTestId("transport-length")).toHaveText("0:08.00");
  const ph = page.getByTestId("timeline-playhead");
  for (const [position, readout] of [
    [500_000, "0:00.15"],
    [2_500_000, "0:02.15"],
    [4_000_000, "0:04.00"],
  ]) {
    await mock.event("kv://position", { position, playing: true, frame_us: FRAME });
    await expect(ph).toHaveAttribute("data-position", String(position));
    await expect(page.getByTestId("transport-time")).toHaveText(readout);
  }
  await expect(page.getByTestId("transport-play")).toHaveAttribute("data-playing", "true");
  // Half way through the 8 s span.
  const content = await page.getByTestId("timeline-content").boundingBox();
  const box = await ph.boundingBox();
  expect(Math.abs(box.x + box.width / 2 - (content.x + content.width / 2))).toBeLessThan(3);
  // Playback ended.
  await mock.event("kv://position", { position: 8 * SEC, playing: false, frame_us: FRAME });
  await expect(page.getByTestId("transport-play")).toHaveAttribute("data-playing", "false");
  await expect(page.getByTestId("transport-time")).toHaveText("0:08.00");
});

test("dragging on the ruler scrubs with one seek in flight", async ({ page, mock }) => {
  await mock.open(edit, { beats });
  // The first seek answers late; moves meanwhile only keep the newest target.
  await mock.respond("seek", null, { delayMs: 500 });
  const ruler = await page.getByTestId("timeline-ruler").boundingBox();
  const y = ruler.y + ruler.height / 2;
  const x = (f) => ruler.x + ruler.width * f;
  await page.mouse.move(x(0.1), y);
  await page.mouse.down();
  for (const f of [0.2, 0.3, 0.4, 0.5]) await page.mouse.move(x(f), y);
  // The playhead follows the pointer at once...
  const ph = page.getByTestId("timeline-playhead");
  await expect
    .poll(async () => Number(await ph.getAttribute("data-position")))
    .toBeGreaterThan(3.9 * SEC);
  // ...but only the first seek has been sent.
  expect(await mock.commands("seek")).toHaveLength(1);
  // Position events from before the seek do not pull the playhead back.
  await mock.event("kv://position", { position: 0, playing: false, frame_us: FRAME });
  expect(Number(await ph.getAttribute("data-position"))).toBeGreaterThan(3.9 * SEC);
  await page.mouse.up();
  // When it answers, the newest target follows: two seeks in all.
  await expect.poll(() => mock.commands("seek")).toHaveLength(2);
  const [first, last] = await seeks(mock);
  expect(Math.abs(first - 0.8 * SEC)).toBeLessThan(0.02 * SEC);
  expect(Math.abs(last - 4 * SEC)).toBeLessThan(0.02 * SEC);
  await page.waitForTimeout(100);
  expect(await mock.commands("seek")).toHaveLength(2);
  await expect(ph).toHaveAttribute("data-position", String(last));
});

test("ctrl+shift+d toggles the debug overlay and logs seek latency", async ({ page, mock }) => {
  await mock.open(edit);
  await page.evaluate(() => {
    window.__kvMock.stats = {
      backend: "native-metal",
      engine: true,
      presented: 120,
      dropped: 2,
      seek: { count: 40, last_ms: 31.5, p50_ms: 28.25, p95_ms: 61 },
    };
  });
  const logs = [];
  page.on("console", (m) => logs.push(m.text()));
  const overlay = page.getByTestId("preview-debug");
  await expect(overlay).toBeHidden();
  await page.keyboard.press("Control+Shift+D");
  await expect(overlay).toBeVisible();
  await expect(overlay).toContainText("p50 28.3 ms");
  await expect(overlay).toContainText("p95 61.0 ms");
  await expect(overlay).toContainText("n=40");
  await expect(overlay).toContainText("120 shown, 2 dropped");
  await expect.poll(() => logs.some((l) => /p50 28\.3 ms, p95 61\.0 ms over 40 seeks/.test(l))).toBe(
    true,
  );
  await page.keyboard.press("Control+Shift+D");
  await expect(overlay).toBeHidden();
  // Polling stops with the overlay.
  const polls = (await mock.commands("get_preview_stats")).length;
  await page.waitForTimeout(700);
  expect(await mock.commands("get_preview_stats")).toHaveLength(polls);
});

test("transport keys are ignored while a dialog is open", async ({ page, mock }) => {
  await mock.open(edit);
  await page.evaluate(() => {
    const d = document.createElement("div");
    d.setAttribute("role", "alertdialog");
    document.body.append(d);
  });
  await page.keyboard.press("Space");
  await page.keyboard.press("ArrowRight");
  await page.waitForTimeout(100);
  expect(await mock.commands("play")).toHaveLength(0);
  expect(await mock.commands("seek")).toHaveLength(0);
});
