# kv-app: the kwikvid desktop app

A thin Tauri 2 shell over [`kv-session`](../kv-session): all app logic lives
there, this crate only forwards IPC calls and pushes state to the UI. Binary:
`kwikvid-app`.

## Run it

macOS (Apple Silicon or Intel): needs Rust stable and FFmpeg (`brew install
ffmpeg pkgconf`, as for the rest of the workspace).

```sh
cargo run -p kv-app
```

Linux: also needs the WebKitGTK stack:

```sh
sudo apt-get install libwebkit2gtk-4.1-dev libgtk-3-dev librsvg2-dev
cargo run -p kv-app
# headless (CI, cloud sessions): start, wait for the first state event, exit 0
xvfb-run -a cargo run -p kv-app -- --smoke
```

`--smoke` exits with code 0 once the first `kv://state` event has been emitted
(after the page loads), or with code 3 if none arrives within 60 s.
Log level: `RUST_LOG=debug cargo run -p kv-app`.

## Layout

| Path | What |
| --- | --- |
| `src/main.rs` | Tauri builder, the `apply` / `undo` / `redo` / `get_state` commands, `--smoke`. |
| `src/ipc.rs` | The handlers as plain functions over the `Mutex<Session>` (unit-tested without a window). |
| `tauri.conf.json` | Window (transparent, for the native preview layer of ADR 0007), `macOSPrivateApi`, CSP. |
| `capabilities/default.json` | Permissions of the main window (`core:default`). |
| `ui/` | The frontend: plain HTML, CSS and ES modules, **no bundler, no npm**. Served as-is. |
| `ui/js/api.js` | The only module that touches `window.__TAURI__`. Exports `apply(cmd)`, `undo()`, `redo()`, `getState()`, `onState(cb)`. Swap it for a mock in UI tests. |
| `ui/js/main.js` | Keeps the latest snapshot (dropping stale revisions) and re-renders. |
| `ui-tests/` | Playwright UI tests on a mock backend, plus the tauri-driver smoke (ADR 0011). See AGENTS.md "UI tests". |
| `tests/ui_fixtures.rs` | Writes / checks `ui-tests/fixtures/*.json` from real snapshots. |

## IPC contract

- `apply({ cmd })`: `cmd` is a `kv_core::Command` in its tagged JSON form,
  e.g. `{ "type": "set_seed", "section": 1, "seed": 7 }`.
- `undo()`, `redo()`, `get_state()`.
- Every call resolves to a `kv_session::Snapshot` (snake_case keys:
  `revision`, `project`, `can_undo`, `can_redo`, `dirty`, `media_root`) or
  rejects with `{ "message": "..." }`, which the UI shows as a toast.
- After each change (and on page load) the backend emits `kv://state` with
  the new snapshot to every window.

## Adding UI features

Put each feature in its own `ui/js/<feature>.js` and `ui/css/<feature>.css`
and touch `ui/index.html` / `ui/js/main.js` with a one-line include or hook
only, so parallel UI work does not conflict.
Test it in its own `ui-tests/tests/<feature>.spec.js`.
