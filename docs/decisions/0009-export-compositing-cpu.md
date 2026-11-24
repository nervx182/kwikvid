# 0009 Export compositing on the CPU; wgpu for preview

Status: accepted (2026-11-24)

## Context

The M1 gate is a headless CLI export: clips + a song in, a beat-synced MP4
out (`docs/roadmap.md`). `kv-render`'s frame plan (#26) lists, for each
output frame, up to two source layers with blend weights (a hard cut has
one, a crossfade two). Something has to turn decoded `kv_media::VideoFrame`s
(RGBA8, #23) into the output frame: place sources of another aspect ratio
(4:3 GoPro footage into a 16:9 export, 16:9 into 9:16 later), and blend.

ADR 0007 puts the M2 live preview on wgpu (Metal on macOS). Export could use
the same GPU path, but M1 export must behave identically in Linux CI (no
GPU) and on macOS, and must be testable bit for bit.

## Decision

- Export compositing for M1 is a small CPU compositor,
  `kv_render::composite(layers, out, fit)`, with `Fit::Fill` (cover, centre
  crop; default) and `Fit::Fit` (letterbox/pillarbox with opaque black).
- Scaling is bilinear with pixel-centre alignment, in gamma-encoded 8-bit
  sRGB values (no linearisation), accumulated in `f32` and rounded once.
  `out = sum(weight_i * fitted_layer_i)`; uncovered areas count as opaque
  black for that layer. One weight-1 layer at the output size is an exact
  copy.
- Heavy scaling belongs in the decoder: `kv_render::source_size_for` gives
  the size to ask the decoder for (#28), so the compositor mostly crops or
  copies. That also avoids bilinear aliasing on big downscales (4K sources).
- Rows run in parallel on `std::thread::scope` (no new dependency).
- The live preview stays on wgpu (ADR 0007). The CPU compositor is the
  reference the GPU path is golden-tested against later.

## Consequences

- No GPU, driver or platform code in export; CI and macOS produce the same
  pixels, and tests are plain unit tests.
- Crossfades blend in gamma space, so they dip slightly darker mid-way than a
  linear-light blend would. Acceptable for short Quik-style crossfades;
  revisit with colour management/HDR.
- A 1080p two-layer composite costs a few milliseconds per frame on a
  4-core runner (logged by `timing_1080p_two_layer_crossfade`), small next
  to decode and H.264 encode. If export later needs more, the GPU path can
  take over behind the same function signature.
- When export moves to the GPU, its output must match this compositor within
  a small tolerance; differences are bugs in one of them.
