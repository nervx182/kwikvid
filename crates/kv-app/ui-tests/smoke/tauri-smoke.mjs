// End-to-end smoke test of the real kv-app (ADR 0011): launches the built
// binary through tauri-driver + WebKitWebDriver and checks the window title,
// the library region, and that the UI rendered a snapshot from the real
// backend (so IPC works). Linux only (WebKitGTK); needs a display.
//
//   cargo build -p kv-app
//   cargo install tauri-driver --locked
//   sudo apt-get install webkitgtk-webdriver xvfb
//   xvfb-run -a node smoke/tauri-smoke.mjs
//
// Env: KV_APP_BIN (default <repo>/target/debug/kwikvid-app),
//      TAURI_DRIVER (default tauri-driver on PATH).
//
// Speaks the W3C WebDriver protocol with fetch, so it needs no npm packages.

import { spawn } from "node:child_process";
import { existsSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const repo = resolve(here, "../../../..");
const app = process.env.KV_APP_BIN || join(repo, "target/debug/kwikvid-app");
const driverBin = process.env.TAURI_DRIVER || "tauri-driver";
const PORT = 4444;
const BASE = `http://127.0.0.1:${PORT}`;
const ELEMENT = "element-6066-11e4-a52e-4f735466cecf";

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

async function wd(method, path, body) {
  const res = await fetch(BASE + path, {
    method,
    headers: { "content-type": "application/json" },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  const json = await res.json().catch(() => ({}));
  if (!res.ok) {
    throw new Error(`${method} ${path}: HTTP ${res.status} ${JSON.stringify(json.value ?? json)}`);
  }
  return json.value;
}

/** Retry fn until it returns a truthy value or the deadline passes. */
async function until(what, fn, ms = 30_000) {
  const end = Date.now() + ms;
  let last;
  while (Date.now() < end) {
    try {
      const v = await fn();
      if (v) return v;
    } catch (e) {
      last = e;
    }
    await sleep(250);
  }
  throw new Error(`timed out waiting for ${what}${last ? `: ${last.message}` : ""}`);
}

function check(cond, msg) {
  if (!cond) throw new Error(`FAIL: ${msg}`);
  console.log(`ok: ${msg}`);
}

async function main() {
  if (!existsSync(app)) throw new Error(`${app} not found; run cargo build -p kv-app first`);
  const driver = spawn(driverBin, ["--port", String(PORT)], { stdio: "inherit" });
  driver.on("error", (e) => {
    console.error(`could not start ${driverBin}: ${e.message}`);
    process.exit(2);
  });
  let session;
  try {
    await until("tauri-driver", () => wd("GET", "/status").then(() => true));
    const created = await until(
      "a WebDriver session",
      () =>
        wd("POST", "/session", {
          capabilities: {
            alwaysMatch: { browserName: "wry", "tauri:options": { application: app } },
          },
        }),
      60_000,
    );
    session = created.sessionId;
    const s = `/session/${session}`;

    const title = await until("the page title", () => wd("GET", `${s}/title`));
    check(title === "kwikvid", `window title is "kwikvid" (got "${title}")`);

    const lib = await until("the library region", () =>
      wd("POST", `${s}/element`, { using: "css selector", value: '[data-testid="library"]' }),
    );
    const shown = await wd("GET", `${s}/element/${lib[ELEMENT]}/displayed`);
    check(shown === true, "library region is displayed");

    const inspector = await until("a rendered snapshot", async () => {
      const el = await wd("POST", `${s}/element`, {
        using: "css selector",
        value: "#inspector-summary",
      });
      const text = await wd("GET", `${s}/element/${el[ELEMENT]}/text`);
      return text.includes("Revision") ? text : null;
    });
    check(true, `UI rendered the backend snapshot (${inspector.replace(/\s+/g, " ").trim()})`);
  } finally {
    if (session) await wd("DELETE", `/session/${session}`).catch(() => {});
    driver.kill();
  }
}

main().then(
  () => console.log("smoke: passed"),
  (e) => {
    console.error(e.message);
    process.exit(1);
  },
);
