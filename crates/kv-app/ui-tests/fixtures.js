// Shared test setup: import { test, expect, snapshots } from "../fixtures.js".
//
// Every test gets a `mock` fixture that serves mock-api.js in place of
// ui/js/api.js. Typical test:
//
//   test("undo is enabled after an edit", async ({ page, mock }) => {
//     await mock.open(snapshots.edited);         // load the UI with this state
//     await expect(page.getByTestId("undo")).toBeEnabled();
//     await mock.respond("undo", snapshots.undone); // next undo returns this
//     await page.getByTestId("undo").click();
//     expect(await mock.commands("undo")).toHaveLength(1);
//   });
//
// snapshots.<name> are the JSON files in fixtures/, generated from real
// kv_session::Snapshot values by crates/kv-app/tests/ui_fixtures.rs.

import { readFileSync, readdirSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { test as base, expect } from "@playwright/test";

const here = dirname(fileURLToPath(import.meta.url));
const MOCK_API = join(here, "mock-api.js");

/** Every fixtures/<name>.json, keyed by <name>. */
export const snapshots = Object.fromEntries(
  readdirSync(join(here, "fixtures"))
    .filter((f) => f.endsWith(".json"))
    .map((f) => [f.slice(0, -5), JSON.parse(readFileSync(join(here, "fixtures", f), "utf8"))]),
);

class Mock {
  constructor(page) {
    this.page = page;
  }

  /**
   * Load the UI with `snapshot` as the backend state (default: initial).
   * `beats`: what get_beats answers (default null, "not analysed yet").
   */
  async open(snapshot = snapshots.initial, { beats = null } = {}) {
    await this.page.addInitScript((init) => {
      window.__kvMockInit = init;
    }, { state: snapshot, beats });
    await this.page.goto("/");
    // Fails fast if the real api.js was served instead of the mock.
    await this.page.waitForFunction(() => window.__kvMock !== undefined);
  }

  /** Set the backend state and push it to the UI, like a kv://state event. */
  async state(snapshot) {
    await this.page.evaluate((s) => window.__kvMock.emit(s), snapshot);
  }

  /**
   * The calls the UI made, in order, as { command, args? }; command is
   * "apply", "undo", "redo", "get_state", "get_beats" or "seek". Pass a
   * name to filter.
   */
  async commands(name) {
    const all = await this.page.evaluate(() => window.__kvMock.commands);
    return name === undefined ? all : all.filter((c) => c.command === name);
  }

  /** Set what get_beats answers from now on (null: not analysed yet). */
  async beats(beats) {
    await this.page.evaluate((b) => (window.__kvMock.beats = b), beats);
  }

  /** The next `command` call resolves to `snapshot` (it becomes the state). */
  async respond(command, snapshot) {
    await this.page.evaluate((r) => window.__kvMock.responses.push(r), { command, snapshot });
  }

  /** The next `command` call rejects with { message }. */
  async fail(command, message) {
    await this.page.evaluate((r) => window.__kvMock.responses.push(r), {
      command,
      error: message,
    });
  }
}

export const test = base.extend({
  mock: async ({ page }, use) => {
    await page.route("**/js/api.js", (route) =>
      route.fulfill({ path: MOCK_API, contentType: "text/javascript" }),
    );
    // Surface UI exceptions as test failures instead of silent no-ops.
    const errors = [];
    page.on("pageerror", (e) => errors.push(e));
    await use(new Mock(page));
    expect(errors, "uncaught errors in the page").toEqual([]);
  },
});

export { expect };
