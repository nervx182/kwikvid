// Music picker: bundled track list, choose -> analysing -> set (driven by
// kv://job events), the error state, the song's name / duration / tempo /
// beat strip, and remove.
//
// music-set (see crates/kv-app/tests/ui_fixtures.rs): Music/song.mp3, 8 s,
// set on the default section. music-removed: the same, then removed.
// beats-song: 120 BPM, a beat every 0.5 s, a downbeat every 2 s.

import { test, expect, snapshots } from "../fixtures.js";

const SEC = 1_000_000;
const initial = snapshots.initial;
const set = snapshots["music-set"];
const removed = snapshots["music-removed"];
const beats = snapshots["beats-song"];

const BUNDLED = [
  { id: "arpent", title: "Arpent", artist: "Kevin MacLeod", path: "/app/music/arpent.mp3" },
  { id: "beat-one", title: "Beat One", artist: "Kevin MacLeod", path: "/app/music/beat-one.mp3" },
];

/** A kv://job update for the music analysis job `id`. */
const job = (id, state, extra = {}) => ({
  id,
  kind: "music-analysis",
  media_id: null,
  state,
  fraction: null,
  message: null,
  ...extra,
});

test("bundled tracks are listed with title and artist", async ({ page, mock }) => {
  await mock.open(initial, { values: { bundled_tracks: BUNDLED } });
  const tracks = page.getByTestId("music-track");
  await expect(tracks).toHaveCount(2);
  await expect(tracks.locator(".mu-track-title")).toHaveText(["Arpent", "Beat One"]);
  await expect(tracks.locator(".mu-track-artist")).toHaveText(["Kevin MacLeod", "Kevin MacLeod"]);
  await expect(page.getByTestId("music-bundled-heading")).toBeVisible();
  expect(await mock.commands("bundled_tracks")).toHaveLength(1);
});

test("no bundled tracks: no list and no error", async ({ page, mock }) => {
  await mock.open(initial, { values: { bundled_tracks: [] } });
  await expect(page.getByTestId("music-choose")).toBeVisible();
  await expect(page.getByTestId("music-track")).toHaveCount(0);
  await expect(page.getByTestId("music-bundled-heading")).toBeHidden();
  await expect(page.getByTestId("music-error")).toBeHidden();
  await expect(page.getByTestId("music-empty")).toHaveText("No music yet.");
  await expect(page.getByTestId("music-remove")).toBeDisabled();
});

test("choose file -> analysing -> music set", async ({ page, mock }) => {
  await mock.open(initial);
  await mock.reply("pick_music", "/home/ana/Music/song.mp3");
  await mock.reply("analyze_music", job(7, "queued"));
  await page.getByTestId("music-choose").click();

  await expect(page.getByTestId("music-status")).toBeVisible();
  await expect(page.getByTestId("music-status-text")).toHaveText("Analysing song.mp3…");
  expect(await mock.commands("analyze_music")).toEqual([
    { command: "analyze_music", args: { section: 1, path: "/home/ana/Music/song.mp3" } },
  ]);
  await expect(page.getByTestId("music-choose")).toBeDisabled();

  await mock.event("kv://job", job(7, "started"));
  await mock.event("kv://job", job(7, "progress", { fraction: 0.4 }));
  await expect(page.getByTestId("music-status-text")).toHaveText("Analysing song.mp3… 40%");
  // Other jobs are ignored.
  await mock.event("kv://job", job(8, "failed", { message: "not mine" }));
  await expect(page.getByTestId("music-error")).toBeHidden();

  // The backend applies SetMusic, emits kv://state, then the job's "done".
  await mock.beats(beats);
  await mock.state(set);
  await mock.event("kv://job", job(7, "done"));
  await expect(page.getByTestId("music-status")).toBeHidden();
  await expect(page.getByTestId("music-choose")).toBeEnabled();
  await expect(page.getByTestId("music-title")).toHaveText("song.mp3");
  await expect(page.getByTestId("music-duration")).toHaveText("0:08");
  await expect(page.getByTestId("music-tempo")).toHaveText("120 BPM");
  await expect(page.getByTestId("music-beat")).toHaveCount(16);
  // The UI never sets the music itself.
  expect(await mock.commands("apply")).toEqual([]);
});

test("a bundled track is analysed by its resource path", async ({ page, mock }) => {
  await mock.open(initial, { values: { bundled_tracks: BUNDLED } });
  await mock.reply("analyze_music", job(3, "queued"));
  await page.getByTestId("music-track").nth(1).click();
  await expect(page.getByTestId("music-status-text")).toHaveText("Analysing Beat One…");
  expect((await mock.commands("analyze_music"))[0].args).toEqual({
    section: 1,
    path: "/app/music/beat-one.mp3",
  });
  await expect(page.getByTestId("music-track").first()).toBeDisabled();
});

test("analysis failure shows the error and keeps the music", async ({ page, mock }) => {
  await mock.open(set, { beats });
  await expect(page.getByTestId("music-title")).toHaveText("song.mp3");
  await mock.reply("pick_music", "/home/ana/Music/voice.flac");
  await mock.reply("analyze_music", job(2, "queued"));
  await page.getByTestId("music-choose").click();
  await expect(page.getByTestId("music-status")).toBeVisible();
  const message = 'Couldn\'t analyse the music in "voice.flac": unsupported audio: opus';
  await mock.event("kv://job", job(2, "failed", { message }));
  await expect(page.getByTestId("music-status")).toBeHidden();
  await expect(page.getByTestId("music-error")).toHaveText(message);
  await expect(page.getByTestId("music-title")).toHaveText("song.mp3");
  await expect(page.getByTestId("music-tempo")).toHaveText("120 BPM");
});

test("a refused pick shows the error without analysing", async ({ page, mock }) => {
  await mock.open(initial);
  await mock.reply("pick_music", "/home/ana/a.ogg");
  await mock.fail("analyze_music", '"a.ogg" is not a supported song. Use MP3, AAC, WAV or FLAC.');
  await page.getByTestId("music-choose").click();
  await expect(page.getByTestId("music-error")).toHaveText(
    '"a.ogg" is not a supported song. Use MP3, AAC, WAV or FLAC.',
  );
  await expect(page.getByTestId("music-status")).toBeHidden();
  await expect(page.getByTestId("music-empty")).toBeVisible();
});

test("a cancelled picker does nothing", async ({ page, mock }) => {
  await mock.open(initial);
  await mock.reply("pick_music", null);
  await page.getByTestId("music-choose").click();
  await expect.poll(() => mock.commands("pick_music")).toHaveLength(1);
  expect(await mock.commands("analyze_music")).toEqual([]);
  await expect(page.getByTestId("music-status")).toBeHidden();
});

test("a cached analysis finishing before analyze_music resolves still completes", async ({
  page,
  mock,
}) => {
  await mock.open(initial);
  await mock.reply("pick_music", "/home/ana/Music/song.mp3");
  // Hold the answer back and deliver "done" first.
  await page.evaluate(() => {
    window.__kvMock.responses.push({
      command: "analyze_music",
      value: { id: 5, kind: "music-analysis", media_id: null, state: "queued" },
      delayMs: 300,
    });
  });
  await page.getByTestId("music-choose").click();
  await expect(page.getByTestId("music-status")).toBeVisible();
  await mock.beats(beats);
  await mock.state(set);
  await mock.event("kv://job", job(5, "done"));
  await expect(page.getByTestId("music-status")).toBeHidden();
  await expect(page.getByTestId("music-title")).toHaveText("song.mp3");
});

test("the beat strip covers 30 s with downbeats emphasised", async ({ page, mock }) => {
  // 40 s at 120 BPM: only the 60 beats before 30 s are drawn (0 .. 29.5 s, plus 30 s).
  const long = {
    music: "Music/song.mp3",
    duration: 40 * SEC,
    tempo_bpm: 119.6,
    beats: [...Array(80).keys()].map((i) => i * 500_000),
    downbeats: [...Array(20).keys()].map((i) => i * 2_000_000),
  };
  await mock.open(set, { beats: long });
  await expect(page.getByTestId("music-tempo")).toHaveText("120 BPM");
  const ticks = page.getByTestId("music-beat");
  await expect(ticks).toHaveCount(61);
  await expect(page.locator("[data-testid=music-beat][data-downbeat=true]")).toHaveCount(16);
  const strip = await page.getByTestId("music-beats").boundingBox();
  const at = async (t) => {
    const b = await page.locator(`[data-testid=music-beat][data-time="${t}"]`).boundingBox();
    return (b.x - strip.x) / strip.width;
  };
  expect(Math.abs((await at(15 * SEC)) - 0.5)).toBeLessThan(0.01);
  const down = await page.locator(`[data-testid=music-beat][data-time="${2 * SEC}"]`).boundingBox();
  const beat = await page.locator(`[data-testid=music-beat][data-time="${2.5 * SEC}"]`).boundingBox();
  expect(down.height).toBeGreaterThan(beat.height);
});

test("tempo shows a placeholder until the analysis is available", async ({ page, mock }) => {
  await mock.open(set);
  await expect(page.getByTestId("music-title")).toHaveText("song.mp3");
  await expect(page.getByTestId("music-tempo")).toHaveText("– BPM");
  await expect(page.getByTestId("music-beat")).toHaveCount(0);
});

test("remove music sends SetMusic with no music", async ({ page, mock }) => {
  await mock.open(set, { beats });
  await mock.respond("apply", removed);
  await page.getByTestId("music-remove").click();
  expect(await mock.commands("apply")).toEqual([
    { command: "apply", args: { cmd: { type: "set_music", section: 1, music: null } } },
  ]);
  await expect(page.getByTestId("music-empty")).toBeVisible();
  await expect(page.getByTestId("music-current")).toBeHidden();
  await expect(page.getByTestId("music-remove")).toBeDisabled();
});
