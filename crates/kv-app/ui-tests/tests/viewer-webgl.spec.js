// WebGL preview fallback (ADR 0007, #76): with the "webgl" backend the
// viewer draws kvframe:// frames on a canvas, letterboxed to the project's
// aspect ratio. The frame protocol is mocked with page.route (mock-api.js
// points frameUrl at http://kvframe.localhost, the Windows form).

import { test, expect, snapshots } from "../fixtures.js";

const WEBGL = { values: { get_preview_backend: "webgl" } };

/** Raw RGBA of a w x h frame in one colour. */
function solid(w, h, rgba) {
  const buf = Buffer.alloc(w * h * 4);
  for (let i = 0; i < buf.length; i += 4) buf.set(rgba, i);
  return buf;
}

/**
 * Serve `frames` (a list of { rgba, w, h }, sequence numbers 1, 2, ...) on
 * the frame protocol, like src/webgl.rs: the newest frame after `after`,
 * else 204. Returns the `after` values requested.
 */
async function serveFrames(page, frames) {
  const asked = [];
  await page.route("http://kvframe.localhost/**", (route) => {
    const after = Number(new URL(route.request().url()).searchParams.get("after"));
    asked.push(after);
    const headers = {
      "Access-Control-Allow-Origin": "*",
      "Access-Control-Expose-Headers": "X-Frame-Width, X-Frame-Height, X-Frame-Seq, X-Frame-Pts",
      "Cache-Control": "no-store",
    };
    const seq = frames.length;
    if (seq <= after) {
      return route.fulfill({ status: 204, headers: { ...headers, "X-Frame-Seq": String(seq) } });
    }
    const f = frames[seq - 1];
    return route.fulfill({
      status: 200,
      contentType: "application/octet-stream",
      headers: {
        ...headers,
        "X-Frame-Width": String(f.w),
        "X-Frame-Height": String(f.h),
        "X-Frame-Seq": String(seq),
        "X-Frame-Pts": "0",
      },
      body: solid(f.w, f.h, f.rgba),
    });
  });
  return asked;
}

async function hasWebGl2(page) {
  return page.evaluate(() => !!document.createElement("canvas").getContext("webgl2"));
}

const pixel = (page, x, y) =>
  page.getByTestId("viewer-canvas").evaluate((c, [x, y]) => c.kvPixel(x, y), [x, y]);

test("no canvas unless the backend is webgl", async ({ page, mock }) => {
  await page.route("http://kvframe.localhost/**", (r) => r.abort());
  await mock.open(snapshots.initial);
  await expect.poll(() => mock.commands("get_preview_backend")).toHaveLength(1);
  await page.waitForTimeout(100);
  await expect(page.getByTestId("viewer-canvas")).toHaveCount(0);
});

test("a solid frame is drawn letterboxed to the project aspect", async ({ page, mock }) => {
  const asked = await serveFrames(page, [{ rgba: [255, 0, 0, 255], w: 32, h: 18 }]);
  await mock.open(snapshots.initial, WEBGL);
  test.skip(!(await hasWebGl2(page)), "this browser build has no WebGL2");
  const canvas = page.getByTestId("viewer-canvas");
  await expect(canvas).toHaveAttribute("data-seq", "1");
  // The canvas covers the viewer, under the transport bar.
  const [cb, vb] = await Promise.all([canvas.boundingBox(), page.getByTestId("viewer").boundingBox()]);
  expect(cb).toEqual(vb);
  await expect(page.getByTestId("transport-play")).toBeVisible();

  expect(await pixel(page, cb.width / 2, cb.height / 2)).toEqual([255, 0, 0, 255]);
  // 16:9 in the viewer: black bars on the sides or at the top and bottom.
  const wide = cb.width / cb.height > 16 / 9;
  const bar = wide ? await pixel(page, 1, cb.height / 2) : await pixel(page, cb.width / 2, 1);
  expect(bar).toEqual([0, 0, 0, 255]);

  // Nothing newer: the backend answers 204 and the UI keeps asking after 1.
  await expect.poll(() => asked.filter((a) => a === 1).length).toBeGreaterThan(2);
  expect(asked.every((a, i) => i === 0 || a >= asked[i - 1])).toBe(true);
  expect(await pixel(page, cb.width / 2, cb.height / 2)).toEqual([255, 0, 0, 255]);
});

test("a newer frame replaces the shown one", async ({ page, mock }) => {
  const frames = [{ rgba: [255, 0, 0, 255], w: 16, h: 9 }];
  await serveFrames(page, frames);
  await mock.open(snapshots.initial, WEBGL);
  test.skip(!(await hasWebGl2(page)), "this browser build has no WebGL2");
  const canvas = page.getByTestId("viewer-canvas");
  await expect(canvas).toHaveAttribute("data-seq", "1");
  frames.push({ rgba: [0, 255, 0, 255], w: 16, h: 9 });
  await expect(canvas).toHaveAttribute("data-seq", "2");
  const b = await canvas.boundingBox();
  expect(await pixel(page, b.width / 2, b.height / 2)).toEqual([0, 255, 0, 255]);
});

test("a portrait project pillarboxes the frame", async ({ page, mock }) => {
  const portrait = structuredClone(snapshots.initial);
  portrait.project.settings.aspect_ratio = "portrait9x16";
  await serveFrames(page, [{ rgba: [0, 0, 255, 255], w: 18, h: 32 }]);
  await mock.open(portrait, WEBGL);
  test.skip(!(await hasWebGl2(page)), "this browser build has no WebGL2");
  const canvas = page.getByTestId("viewer-canvas");
  await expect(canvas).toHaveAttribute("data-seq", "1");
  const b = await canvas.boundingBox();
  expect(await pixel(page, b.width / 2, b.height / 2)).toEqual([0, 0, 255, 255]);
  expect(await pixel(page, 2, b.height / 2)).toEqual([0, 0, 0, 255]);
  expect(await pixel(page, b.width - 2, b.height / 2)).toEqual([0, 0, 0, 255]);
});
