# Tauri 2 preview spike (issue #5)

Throwaway code that answers one question: which way of getting video frames
on screen in a Tauri 2 app sustains 60 fps at 1080p on Apple Silicon? The
answer and the numbers are in
[`docs/decisions/0007-preview-path.md`](../../docs/decisions/0007-preview-path.md).

This crate is **not** a workspace member (`exclude = ["spikes"]` in the root
`Cargo.toml`), so the merge gate never builds it. It has its own `Cargo.lock`.
Do not build on it: production preview code belongs in `kv-render` / `kv-app`.

## What it compares

Every path shows the same synthetic "decoded" frame: scrolling colour bars, a
moving box and a 6-digit frame counter, generated on the CPU in Rust
(`src/pattern.rs`).

| Path id | How frames reach the screen |
| --- | --- |
| `baseline` | No frames; `requestAnimationFrame` only. Shows the display/compositor rate. |
| `native-wgpu` | wgpu draws into a `CAMetalLayer` on an `NSView` placed **under** the transparent WKWebView. The HTML HUD stays on top. macOS only; skipped elsewhere. |
| `raw-1080` | Raw 1920x1080 RGBA (8 MB/frame) over the `kvframe://` custom protocol, uploaded to a WebGL2 texture. |
| `jpeg-1080` | 1920x1080 JPEG (q80, encoded in Rust) over `kvframe://`, decoded with `createImageBitmap`, drawn with WebGL2. |
| `raw-540` | Like `raw-1080` at 960x540 (a half-res proxy), upscaled by the GPU. |

Measured per path, after a 1 s warm-up:

- **fps**: distinct frames that reached the screen per second (`produced` =
  frames received; the gap is frames superseded before they could be shown).
- **latency**: web paths: from the moment the frame was requested (Rust
  generates it on request) to the first `requestAnimationFrame` after it was
  drawn. Native path: from frame generation start to GPU work done for that
  frame; add about one vsync for scan-out.
- **interval** p50/p95 between displayed frames, and **long** frames (> 25 ms).
- **gen**: Rust-side cost to produce a frame (and encode it, for JPEG).

## Run it

Prerequisites: Rust stable. Linux also needs
`libwebkit2gtk-4.1-dev libgtk-3-dev librsvg2-dev`.

```sh
cd spikes/tauri-preview
cargo run --release                     # interactive: click a path or "Run all"
cargo run --release -- --bench          # every path for 10 s, write log, exit
cargo run --release -- --bench --seconds 5 --only raw-1080,native-wgpu --log /tmp/bench.log
```

Options (also `KV_SPIKE_BENCH=1`, `KV_SPIKE_LOG=path`):

- `--bench`: run the paths automatically, then exit (code 0 on success).
  A watchdog exits with code 3 if the run hangs.
- `--seconds N`: seconds per path (default 10).
- `--only a,b`: subset of path ids.
- `--log PATH`: log file (default `~/kv-preview-bench.log`, appended).

Linux without a display: `xvfb-run -a -s "-screen 0 1920x1080x24" target/release/kv-preview-spike --bench`.
Numbers under Xvfb use software GL and say nothing about real hardware.

### Log format

Plain text, one event per line: `START`, `WINDOW`, `ENV` (user agent, device
pixel ratio, WebGL2/WebCodecs availability), `RUN`, `SKIP`, `SUMMARY` (human
readable), `RESULT` (the same as JSON), `END code=N`.

## The Mac build from CI

`.github/workflows/preview-spike.yml` builds the unsigned `.app` on
`macos-latest` (Apple Silicon), runs `--bench` on the runner's GUI session and
uploads two artifacts:

- `kv-preview-spike-macos-arm64`: zipped `.app`.
- `preview-bench-macos`: the benchmark log plus two screenshots (may be black
  if the runner does not grant screen recording).

It runs on pull requests that touch this folder or the workflow, and by hand
from Actions > preview-spike > Run workflow.

To run the downloaded app on your own Mac:

1. Download and unzip `kv-preview-spike-macos-arm64`, then unzip the inner
   `kv-preview-spike-macos-arm64.zip` (GitHub wraps artifacts in a zip).
2. The app is unsigned (ADR 0004). Either right-click it > Open > Open, or run
   `xattr -dr com.apple.quarantine kv-preview-spike.app`.
3. Benchmark from Terminal, with the window visible and nothing covering it:

   ```sh
   ./kv-preview-spike.app/Contents/MacOS/kv-preview-spike --bench --log ~/Desktop/preview-bench.log
   ```

   Or open the app normally and click "Run all"; results are appended to
   `~/kv-preview-bench.log`.
4. The `SUMMARY` lines are what we need. A 120 Hz ProMotion display reports
   about 120 fps for `baseline`; that is expected.
