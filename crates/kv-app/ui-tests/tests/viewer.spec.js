// Viewer: the native preview layer (ADR 0007, #75) is kept aligned with the
// viewer element through set_viewer_rect.

import { test, expect, snapshots } from "../fixtures.js";

async function viewerBox(page) {
  return page.getByTestId("viewer").evaluate((el) => {
    const r = el.getBoundingClientRect();
    return { x: r.left, y: r.top, w: r.width, h: r.height, scale: window.devicePixelRatio };
  });
}

test("the viewer rect is sent on load", async ({ page, mock }) => {
  await mock.open(snapshots.initial);
  await expect.poll(() => mock.commands("set_viewer_rect")).toHaveLength(1);
  const [sent] = await mock.commands("set_viewer_rect");
  expect(sent.args).toEqual(await viewerBox(page));
  expect(sent.args.w).toBeGreaterThan(0);
  expect(sent.args.h).toBeGreaterThan(0);
});

test("a window resize sends the new rect, once per change", async ({ page, mock }) => {
  await mock.open(snapshots.initial);
  await expect.poll(() => mock.commands("set_viewer_rect")).toHaveLength(1);
  await page.setViewportSize({ width: 1000, height: 700 });
  await expect.poll(() => mock.commands("set_viewer_rect")).toHaveLength(2);
  const sent = await mock.commands("set_viewer_rect");
  expect(sent[1].args).toEqual(await viewerBox(page));
  expect(sent[1].args.w).toBeLessThan(sent[0].args.w);
  // No change, no message.
  await page.evaluate(() => window.dispatchEvent(new Event("resize")));
  await page.waitForTimeout(200);
  expect(await mock.commands("set_viewer_rect")).toHaveLength(2);
});

test("a layout change that moves the viewer sends the new rect", async ({ page, mock }) => {
  await mock.open(snapshots.initial);
  await expect.poll(() => mock.commands("set_viewer_rect")).toHaveLength(1);
  // Widen the library pane: the viewer moves and shrinks.
  await page.addStyleTag({ content: ".app { grid-template-columns: 400px 1fr 260px !important; }" });
  await expect.poll(() => mock.commands("set_viewer_rect")).toHaveLength(2);
  const sent = await mock.commands("set_viewer_rect");
  expect(sent[1].args).toEqual(await viewerBox(page));
  expect(sent[1].args.x).not.toEqual(sent[0].args.x);
});
