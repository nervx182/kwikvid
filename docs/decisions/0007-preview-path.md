# 0007 Preview path: native wgpu layer under a transparent webview

Status: accepted, provisional until confirmed on a real Mac (2026-11-24)

## Context

ADR 0002 picked Tauri 2 on the condition that an M0 spike proves 60 fps video
preview. Decoded frames live in Rust; the UI is a WKWebView. The question is
how frames get to the screen. The spike (`spikes/tauri-preview/`, issue #5)
draws the same CPU-generated 1920x1080 test pattern (moving bars, moving box,
frame counter) through each candidate path and logs delivered fps and
frame-to-screen latency. No real decoding.

Paths measured:

- **native-wgpu**: wgpu renders into a `CAMetalLayer` on an `NSView` inserted
  *below* the WKWebView; the webview is transparent, so the HTML UI floats on
  top. Each frame is uploaded from the CPU (worst case; VideoToolbox can later
  hand over IOSurfaces with no copy).
- **raw-1080**: raw RGBA (8 MB/frame) served on a `kvframe://` custom protocol,
  fetched by JS and uploaded to a WebGL2 texture.
- **jpeg-1080**: JPEG q80 encoded in Rust, same protocol, decoded by
  `createImageBitmap`, drawn with WebGL2.
- **raw-540**: raw RGBA at 960x540 (a half-res proxy), upscaled by WebGL.
- **baseline**: `requestAnimationFrame` alone, i.e. the display rate.

`<video>` playing proxy H.264 files was not built: it needs real encoded media
and gives up frame-exact control of the edit (cuts, speed ramps, transitions
are applied by seeking), which is the thing a preview must get right.

## Measurements

fps = distinct frames shown per second; produced = frames delivered by Rust
(the surplus is superseded before display). Latency: web paths from frame
request to the first animation frame after drawing it; native path from frame
generation to GPU completion (add about one vsync to glass). Long = display
intervals over 25 ms. 10 s per path after 1 s warm-up.

**Apple Silicon CI runner (virtualised), indicative.** GitHub `macos-latest`:
"Apple M1 (Virtual)", 3 cores, 7 GB, "Apple Paravirtual device" GPU, 1024x768
display at scale 1 (so the window is 1024x674, far smaller than a Retina
window). Three runs of the `preview-spike` workflow on PR #14 (run
45428374218, and run 45430502899 attempts 1 and 2), shown as run1 / run2 /
run3:

| Path | fps | latency p50 ms | latency p95 ms | long frames (% of shown) | produced/s |
| --- | --- | --- | --- | --- | --- |
| baseline | 59.9 / 60.0 / 59.9 | | | 0.2 / 0.2 / 0.3 | |
| native-wgpu | 55.2 / 59.3 / 57.3 | 33 / 33 / 33 | 78 / 36 / 40 | 4.9 / 1.0 / 2.4 | = fps |
| raw-1080 | 52.1 / 58.8 / 57.1 | 35 / 24 / 32 | 61 / 34 / 45 | 10.6 / 0.9 / 6.7 | 161 / 324 / 199 |
| jpeg-1080 | 22.0 / 29.9 / 24.1 | 126 / 84 / 120 | 173 / 123 / 148 | 96 / 72 / 95 | 35 / 54 / 38 |
| raw-540 | 57.0 / 60.0 / 59.4 | 23 / 19 / 21 | 45 / 21 / 32 | 6.7 / 0 / 1.3 | 502 / 1342 / 723 |

Rust-side frame generation took 0.3-1.1 ms per 1080p frame (JPEG encode:
18-26 ms). A screenshot taken during the native path confirmed the Metal layer
shows through the transparent webview with the HTML HUD on top (the screenshot
check is a diagnostic only; one raw-1080 capture came back without the pattern
although that path rendered 57 fps). The runner numbers vary run to run by
several fps: treat them as a lower bound with noise, not a benchmark.

**Linux, Xvfb with software GL (ubuntu-latest), not representative.**
Over the two CI runs: baseline 62, raw-1080 4.9-6.9, jpeg-1080 8.9-11.0,
raw-540 8.8-9.3 fps; the native path is not implemented on Linux. Useful only
to show that the app builds and runs headless.

**Owner's Mac: pending.** Confirmation on a real Apple Silicon Mac is bundled
into the Roadmap I review (#21, "Confirm preview fps on a real Mac", which
explains how to run the CI-built `.app`). Runner numbers above are the only
Mac numbers so far.

## Findings

- The CI VM barely holds 60 fps itself (baseline 59.9-60.0 with jitter) on a
  paravirtual GPU and 3 vCPUs. On it, native-wgpu delivered 55-59 fps and
  raw-1080 52-59 fps; raw-540 reached 57-60. Nothing failed outright, and no
  path is limited by the frame source.
- native-wgpu was the most consistent path (fewest long frames in its worst
  run). Its latency p50 of 33 ms is two vsyncs of swapchain queueing with the
  default `maximum_frame_latency` of 2; lowering it to 1 should cut a frame.
- The custom protocol is not the bottleneck: it delivered 161-324 raw 1080p
  frames per second (1.3-2.7 GB/s). The cost of raw-1080 is the 8 MB texture
  upload inside WebKit per frame, which grows with resolution (4K is 4x) and
  with Retina-sized canvases, and it competes with the rest of the UI for the
  WebKit main thread. The runner's small window flatters it.
- JPEG is out: encode plus decode costs more than a frame budget per frame,
  before any real decoding is added.
- The native layer takes little code in Tauri 2: an `NSView` inserted under
  the webview, `transparent: true` and the `macos-private-api` feature. It does
  not depend on WebKit at all, and it is the only path that allows zero-copy
  VideoToolbox IOSurface -> Metal texture later.

## Decision

Use the **native wgpu/Metal layer under a transparent webview** for the
preview in `kv-render` / `kv-app`. The webview reserves a transparent
rectangle for the viewer and tells Rust its position and size; Rust renders
there with wgpu and owns pacing (vsync, `PresentMode::Fifo`).

Keep **raw frames over the custom protocol into WebGL** as the fallback and
for non-macOS builds, preferably at proxy size (540p held 57 fps on the VM).
It is also the right path for thumbnails and filmstrips.

Do not stream encoded stills (JPEG) to the webview for playback.

## Consequences

- `kv-app` gets a small macOS-only module that inserts and resizes the native
  view; it needs `unsafe` AppKit calls (allowed locally per AGENTS.md) and the
  `macos-private-api` Tauri feature (transparent webview). That feature blocks
  Mac App Store distribution, which is not a goal.
- The viewer rectangle must be kept in sync with the DOM layout (resize,
  scroll, fullscreen); overlays above the video (scrub handles, titles preview)
  are just HTML on top.
- Windows/Linux need their own native-layer work later or use the WebGL
  fallback; both are behind the `GpuContext` trait.
- If the owner's Mac does not hold 60 fps on the native path at Retina size,
  the fallback is to drop Tauri for the viewer and move to iced (wgpu-native UI,
  same `kv-render` code, UI rewritten in Rust; ADR 0002's stated fallback).
  The Command API keeps that switch contained to `kv-app`.
