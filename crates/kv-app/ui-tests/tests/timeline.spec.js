// Timeline: the edit as proportional blocks over the beat grid, a playhead,
// click-to-seek, zoom and empty states.
//
// timeline-edit (see crates/kv-app/tests/ui_fixtures.rs): 8 s target, four
// segments at 0-2 s, 1.5-4 s (0.5 s crossfade in), 4-6 s (clip 3 at 50%,
// covers a pin) and 6-8 s. beats-song: a beat every 0.5 s, downbeat every 2 s.

import { test, expect, snapshots } from "../fixtures.js";

const SEC = 1_000_000;
const edit = snapshots["timeline-edit"];
const beats = snapshots["beats-song"];

/** Left and width of each block as fractions of the content width. */
async function blockBoxes(page) {
  const content = await page.getByTestId("timeline-content").boundingBox();
  const segs = page.locator(".tl-seg");
  const out = [];
  for (let i = 0; i < (await segs.count()); i++) {
    const b = await segs.nth(i).boundingBox();
    out.push([(b.x - content.x) / content.width, b.width / content.width]);
  }
  return out;
}

function expectClose(actual, expected, tol = 0.005) {
  expect(actual.length).toBe(expected.length);
  actual.forEach(([l, w], i) => {
    expect(Math.abs(l - expected[i][0]), `left of segment ${i}`).toBeLessThan(tol);
    expect(Math.abs(w - expected[i][1]), `width of segment ${i}`).toBeLessThan(tol);
  });
}

const EXPECTED = [
  [0 / 8, 2 / 8],
  [1.5 / 8, 2.5 / 8],
  [4 / 8, 2 / 8],
  [6 / 8, 2 / 8],
];

test("segments are drawn proportionally to their timeline length", async ({ page, mock }) => {
  await mock.open(edit, { beats });
  await expect(page.locator(".tl-seg")).toHaveCount(4);
  expectClose(await blockBoxes(page), EXPECTED);
  await expect(page.getByTestId("timeline-length")).toHaveText("0:08.0");
  await expect(page.getByTestId("timeline-target")).toHaveText("0:08.0");
});

test("segments show the clip name on hover and a filmstrip thumbnail", async ({ page, mock }) => {
  await mock.open(edit);
  const seg = page.locator(".tl-seg");
  await expect(seg.nth(0)).toHaveAttribute("title", "GX010001.MP4");
  await expect(seg.nth(1)).toHaveAttribute("title", "GX010002.MP4");
  // The thumbnail is the 12-cell filmstrip cell nearest the source midpoint.
  // The mock serves no images, so failed thumbnails are removed: check the
  // URL the UI computes instead.
  const urls = await page.evaluate(async () => {
    const t = await import("./js/timeline.js");
    return [
      t.thumbUrl(1, t.nearestCell(2 * 1e6, 10 * 1e6)), // seg 0: mid 2 s of 10 s
      t.thumbUrl(2, t.nearestCell(1.25 * 1e6, 6 * 1e6)), // seg 1: mid 1.25 s of 6 s
      t.thumbUrl(3, t.nearestCell(2.5 * 1e6, 8 * 1e6)), // seg 2: mid 2.5 s of 8 s
      t.thumbUrl(1, t.nearestCell(7 * 1e6, 10 * 1e6)), // seg 3: mid 7 s of 10 s
      t.nearestCell(10 * 1e6, 10 * 1e6), // the very end stays in the last cell
    ];
  });
  expect(urls).toEqual(["kvthumb://1/2", "kvthumb://2/2", "kvthumb://3/3", "kvthumb://1/8", 11]);
});

test("crossfade, slow-mo and pin markers", async ({ page, mock }) => {
  await mock.open(edit);
  const seg = page.locator(".tl-seg");
  // Crossfade wedge only on segment 1, 0.5 s of its 2.5 s.
  await expect(page.locator(".tl-xfade")).toHaveCount(1);
  await expect(seg.nth(1).locator(".tl-xfade")).toHaveCount(1);
  const [wedge, block] = [
    await seg.nth(1).locator(".tl-xfade").boundingBox(),
    await seg.nth(1).boundingBox(),
  ];
  expect(Math.abs(wedge.width / block.width - 0.2)).toBeLessThan(0.02);
  // 0.5x badge only on segment 2.
  await expect(page.locator(".tl-badge")).toHaveCount(1);
  await expect(seg.nth(2).locator(".tl-badge")).toHaveText("0.5x");
  // Segment 2 covers the pinned range on clip 3.
  await expect(page.locator(".tl-pin")).toHaveCount(1);
  await expect(seg.nth(2)).toHaveAttribute("data-pinned", "true");
});

test("beat grid comes from get_beats, downbeats emphasised", async ({ page, mock }) => {
  await mock.open(edit, { beats });
  await expect(page.locator(".tl-beat")).toHaveCount(16);
  await expect(page.locator(".tl-downbeat")).toHaveCount(4);
  const calls = await mock.commands("get_beats");
  expect(calls[0]).toEqual({ command: "get_beats", args: { section: edit.project.sections[0].id } });
  // Downbeat at 2 s sits at a quarter of the 8 s span.
  const content = await page.getByTestId("timeline-content").boundingBox();
  const tick = await page.locator('.tl-downbeat[data-t="2000000"]').boundingBox();
  expect(Math.abs((tick.x - content.x) / content.width - 0.25)).toBeLessThan(0.005);
});

test("beats appear once the analysis finishes", async ({ page, mock }) => {
  await mock.open(edit);
  await expect.poll(() => mock.commands("get_beats")).toHaveLength(1);
  await expect(page.locator(".tl-beat")).toHaveCount(0);
  // The analysis finishes; the next state change asks again.
  await mock.beats(beats);
  await mock.state({ ...edit, revision: edit.revision + 1 });
  await expect(page.locator(".tl-beat")).toHaveCount(16);
  // Once known, the beats are not fetched again for the same music.
  await mock.state({ ...edit, revision: edit.revision + 2 });
  await expect(page.locator(".tl-seg")).toHaveCount(4);
  expect(await mock.commands("get_beats")).toHaveLength(2);
});

test("clicking the ruler sends seek and moves the playhead", async ({ page, mock }) => {
  await mock.open(edit, { beats });
  const ruler = await page.getByTestId("timeline-ruler").boundingBox();
  await page.mouse.click(ruler.x + ruler.width * 0.5, ruler.y + ruler.height / 2);
  await expect.poll(() => mock.commands("seek")).toHaveLength(1);
  const [{ args }] = await mock.commands("seek");
  expect(Math.abs(args.position - 4 * SEC)).toBeLessThan(0.02 * SEC);
  const ph = page.getByTestId("timeline-playhead");
  await expect(ph).toHaveAttribute("data-position", String(args.position));
  const box = await ph.boundingBox();
  expect(Math.abs(box.x + box.width / 2 - (ruler.x + ruler.width * 0.5))).toBeLessThan(3);
});

test("a failed seek shows a toast", async ({ page, mock }) => {
  await mock.open(edit);
  await mock.fail("seek", "Preview is not ready.");
  await page.getByTestId("timeline-ruler").click();
  await expect(page.getByTestId("toasts")).toContainText("Preview is not ready.");
});

test("zoom in widens the content, fit restores it", async ({ page, mock }) => {
  await mock.open(edit);
  const scroll = await page.locator("#timeline-scroll").boundingBox();
  await expect(page.getByTestId("timeline-zoom-out")).toBeDisabled();
  await page.getByTestId("timeline-zoom-in").click();
  await page.getByTestId("timeline-zoom-in").click();
  await expect(page.getByTestId("timeline-view")).toHaveAttribute("data-zoom", "4");
  const wide = await page.getByTestId("timeline-content").boundingBox();
  expect(Math.abs(wide.width / scroll.width - 4)).toBeLessThan(0.05);
  // Proportions are unchanged at any zoom.
  expectClose(await blockBoxes(page), EXPECTED);
  await page.getByTestId("timeline-zoom-out").click();
  await expect(page.getByTestId("timeline-view")).toHaveAttribute("data-zoom", "2");
  await page.getByTestId("timeline-zoom-fit").click();
  await expect(page.getByTestId("timeline-view")).toHaveAttribute("data-zoom", "1");
  const fit = await page.getByTestId("timeline-content").boundingBox();
  expect(Math.abs(fit.width - scroll.width)).toBeLessThan(2);
});

test("empty state: no clips", async ({ page, mock }) => {
  await mock.open(snapshots.initial);
  await expect(page.getByTestId("timeline-empty")).toHaveText("Import clips to start an edit.");
  await expect(page.locator(".tl-seg")).toHaveCount(0);
  expect(await mock.commands("get_beats")).toEqual([]);
});

test("empty state: clips but no music", async ({ page, mock }) => {
  await mock.open(snapshots["timeline-no-music"]);
  await expect(page.getByTestId("timeline-empty")).toHaveText("Add music to create an edit.");
});

test("regenerate error is shown from the session", async ({ page, mock }) => {
  const s = snapshots["timeline-regen-error"];
  expect(s.regen_error).toBeTruthy();
  await mock.open(s);
  await expect(page.getByTestId("timeline-error")).toHaveText(s.regen_error);
  await expect(page.getByTestId("timeline-empty")).toBeHidden();
  // The next state without an error clears it.
  await mock.state({ ...edit, revision: s.revision + 1 });
  await expect(page.getByTestId("timeline-error")).toBeHidden();
});

test("the edit renders when it arrives after an empty state", async ({ page, mock }) => {
  await mock.open(snapshots.initial);
  await mock.state(edit);
  await expect(page.locator(".tl-seg")).toHaveCount(4);
  await expect(page.getByTestId("timeline-empty")).toBeHidden();
});

test("200 segments render quickly", async ({ page, mock }) => {
  // 200 half-second cuts of clip 1 (built from the
  // real fixture's shape).
  const many = structuredClone(edit);
  const sec = many.project.sections[0];
  const proto = sec.edit.segments[0];
  sec.edit.segments = Array.from({ length: 200 }, (_, i) => ({
    ...proto,
    src_in: (i % 19) * 500_000,
    src_out: (i % 19) * 500_000 + 500_000,
    timeline_start: i * 500_000,
    transition: { type: "cut" },
  }));
  sec.target_length = { type: "fixed", length: 100 * SEC };
  many.revision = edit.revision + 1;
  await mock.open(edit, { beats });
  await expect(page.locator(".tl-seg")).toHaveCount(4);
  const ms = await page.evaluate((s) => {
    const t0 = performance.now();
    window.__kvMock.emit(s);
    // Force layout so the measurement includes it.
    document.getElementById("timeline-track").getBoundingClientRect();
    return performance.now() - t0;
  }, many);
  await expect(page.locator(".tl-seg")).toHaveCount(200);
  expect(ms).toBeLessThan(250);
});
