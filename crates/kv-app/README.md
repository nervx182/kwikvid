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

`--preview-test-pattern` plays a moving test pattern (scrolling colour bars,
a bouncing box, the frame number in binary along the bottom) in the viewer
through the preview backend, for manual checks:
`cargo run -p kv-app -- --preview-test-pattern`.

Preview backend: the native Metal layer on macOS, the WebGL fallback (raw
frames at most 960x540 over `kvframe://` into a WebGL2 canvas, ADR 0007)
elsewhere. `KV_PREVIEW=webgl` forces the fallback (e.g. on macOS),
`KV_PREVIEW=off` turns the preview off.

## Download a CI build (macOS, Apple Silicon)

`.github/workflows/app-bundle.yml` builds an unsigned `kwikvid.app` on every
push to `main` that touches `crates/**` or `Cargo.lock`, and on demand
(Actions > app-bundle > Run workflow, or
`gh workflow run app-bundle.yml --ref <branch>`). Each run launches the bundled
binary with `--smoke` and publishes `kwikvid-macos-arm64.zip`:

- as the **`nightly` prerelease** (Releases > nightly), replaced on every run.
  This is the reliable place, since releases do not use artifact storage;
- as the run artifact `kwikvid-macos-arm64`, kept 7 days (may be missing
  when the account's artifact storage is full; the job summary says so).

To open it (same steps as for the spike, see #21):

```sh
# unzip with Finder or `ditto -x -k kwikvid-macos-arm64.zip .` (keeps symlinks)
xattr -dr com.apple.quarantine kwikvid.app   # unsigned: clear Gatekeeper's quarantine flag
open kwikvid.app
```

If macOS still refuses, right-click the app > Open, or allow it under System
Settings > Privacy & Security.

FFmpeg: the CI build links Homebrew's FFmpeg (a GPL build, for development
only, ADR 0005) and does **not** bundle it, so once the app uses `kv-media`
it needs `brew install ffmpeg` on your Mac until #17 ships our own LGPL
build. While the app binary links any FFmpeg library, the workflow skips the
public `nightly` release and only uploads the artifact (the job summary shows
which). Signing, notarisation and a DMG are out of scope for now.

## Layout

| Path | What |
| --- | --- |
| `src/main.rs` | Tauri builder, the `apply` / `undo` / `redo` / `get_state` commands, `--smoke`. |
| `src/ipc.rs` | The handlers as plain functions over the `Mutex<Session>` (unit-tested without a window). |
| `src/preview.rs` | `PreviewBackend` (the one place the preview backend is chosen), viewer rect and letterbox math, test pattern. Portable. |
| `src/blit.rs` | `SurfaceGpu` (a `GpuContext` that can present) and `Blitter` (draws the last frame letterboxed). Portable, tested headless. |
| `src/native_view.rs` | macOS only: the `NSView` + `CAMetalLayer` under the webview, its wgpu surface (Fifo, one frame of latency) and the `FrameSink` (ADR 0007). |
| `src/webgl.rs` | WebGL fallback: `WebGlSink` keeps the latest CPU frame (capped to 960x540) and the `kvframe://latest?after=<seq>` protocol serves it as raw RGBA. Portable, unit-tested. |
| `ui/js/viewer-webgl.js` | With the `webgl` backend: a canvas in the viewer, polling `kvframe://` each animation frame and drawing it letterboxed with WebGL2. |
| `src/transport.rs` | Transport over the preview engine: `play` / `pause` / `seek` / `get_transport`, `kv://position`, edits handed to `set_edit`, seek latency from the engine's `preview_seek` span (`get_preview_stats`). Unit-tested with a fake engine. |
| `ui/js/transport.js` | Play/pause button, m:ss.ff readout, Space / arrows / Shift+arrows / Home / End, ruler scrubbing (one seek in flight), debug overlay on Cmd/Ctrl+Shift+D. |
| `ui/js/viewer.js` | Sends the viewer rectangle (`set_viewer_rect`) on load, resize, layout change and fullscreen. |
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
- `set_viewer_rect({ x, y, w, h, scale })`: the viewer rectangle in CSS
  pixels relative to the page and `devicePixelRatio`; resolves to `null`.
  The native layer covers it and letterboxes the preview to the project's
  aspect ratio.
- `get_preview_backend()`: `"native-metal"`, `"webgl"` or `"off"`.
- `kvframe://latest?after=<seq>` (Windows: `http://kvframe.localhost/latest?after=<seq>`):
  the newest preview frame newer than `seq` as raw RGBA, with
  `X-Frame-Width`, `X-Frame-Height`, `X-Frame-Seq` and `X-Frame-Pts`
  headers; 204 when nothing newer exists.
- `play()`, `pause()`, `seek({ position })`, `get_transport()`: resolve to
  `{ position, playing, frame_us }` (microseconds). While it changes, the
  backend emits the same object as `kv://position`, at most ~15 times a
  second. `get_preview_stats()`: `{ backend, engine, presented, dropped,
  seek: { count, last_ms, p50_ms, p95_ms } }` for the debug overlay; p50/p95
  are also logged every 20 seeks.
- Other calls resolve to a `kv_session::Snapshot` (snake_case keys:
  `revision`, `project`, `can_undo`, `can_redo`, `dirty`, `media_root`) or
  rejects with `{ "message": "..." }`, which the UI shows as a toast.
- After each change (and on page load) the backend emits `kv://state` with
  the new snapshot to every window.

## Adding UI features

Put each feature in its own `ui/js/<feature>.js` and `ui/css/<feature>.css`
and touch `ui/index.html` / `ui/js/main.js` with a one-line include or hook
only, so parallel UI work does not conflict.
Test it in its own `ui-tests/tests/<feature>.spec.js`.
