# 0011 UI testing: Playwright on a mock backend, one tauri-driver smoke

Status: accepted (2026-11-24)

## Context

Every M2 UI issue needs automated tests, and nobody tests by hand (all
development happens in cloud sessions, AGENTS.md). ADR 0002 picked Tauri
partly because a web UI is testable in a Linux browser. The Actions budget is
tight (#43): macOS minutes cost 10x, so UI tests run on Linux, and only when UI
files change. The UI (`crates/kv-app/ui/`) is plain HTML and ES modules with
no bundler, and `ui/js/api.js` is its only link to Tauri (#57).

Options: (1) Playwright against the static `ui/` folder with `api.js` replaced
by a mock; fast, no Rust build, and Playwright's WebKit is close to macOS
WKWebView. (2) `tauri-driver` + WebKitWebDriver against the real app under
`xvfb-run`; full stack, but slow (Rust + WebKitGTK build) and Linux only
(there is no WebDriver for WKWebView). (3) Both.

## Decision

Option 3.

- **Playwright for every UI behaviour.** `crates/kv-app/ui-tests/` holds a
  pinned `@playwright/test` (Apache-2.0), a dependency-free static server
  (`serve.mjs`) and `mock-api.js`, which the `mock` fixture (`fixtures.js`)
  serves in place of `ui/js/api.js` by route interception. Tests set the state
  with `mock.open(snapshot)` / `mock.state(snapshot)`, queue answers with
  `mock.respond(command, snapshot)` / `mock.fail(command, message)`, and read
  what the UI sent with `mock.commands(name?)`. Projects: Chromium and WebKit.
- **One file per feature**: `tests/<feature>.spec.js`, mirroring
  `ui/js/<feature>.js`, so parallel UI issues do not touch the same test file.
- **Fixtures come from real snapshots.** `fixtures/*.json` are written by
  `crates/kv-app/tests/ui_fixtures.rs` from a real `kv_session::Session`; the
  test fails in the merge gate when a checked-in file differs, so the mock
  cannot drift from the backend's JSON. Regenerate with
  `KV_UPDATE_UI_FIXTURES=1 cargo test -p kv-app --test ui_fixtures`.
- **One end-to-end smoke** (`smoke/tauri-smoke.mjs`): builds `kv-app`, launches
  it with `tauri-driver` under `xvfb-run`, and checks the window title, the
  `library` region and that the real backend's snapshot was rendered. It
  speaks WebDriver over `fetch`, so it needs no extra npm packages.
- **Workflow** `.github/workflows/ui-tests.yml`, separate from `ci.yml`:
  `playwright` runs on PRs touching `crates/kv-app/ui/**`, `ui-tests/**` or the
  workflow (10 min timeout, browsers cached by lockfile hash); `tauri-smoke`
  runs only on pushes to `main` that touch the app or the crates it links, and
  on manual dispatch.

## Consequences

- UI tests need no Rust build and run in about a minute; macOS minutes are
  not used. Tauri-specific behaviour (IPC wiring, window config) is covered
  only by the smoke and by kv-app's Rust tests, and WKWebView differences by
  Playwright WebKit plus the `mac-smoke` job.
- New backend commands or `Snapshot` fields need a fixture entry in
  `ui_fixtures.rs`; the mock itself stays generic.
- `playwright` is not a required check at first, because a path-filtered
  workflow that does not run would block PRs that never touch the UI. To make
  it required: drop the `paths` filter (or add an always-running job that
  skips its steps when no UI file changed, e.g. with `dorny/paths-filter`),
  then add its check name to the `main` branch protection next to `gate`.
- No visual regression screenshots and no macOS UI tests (out of scope).
