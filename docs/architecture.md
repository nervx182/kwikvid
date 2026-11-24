# Architecture

One Cargo workspace, a pure-logic core, and three thin front ends (desktop app,
CLI, MCP server) that all drive the engine through the same serialisable
`Command` API.

```
 kv-app (Tauri 2)     kv-cli              kv-mcp (rmcp; stdio + local HTTP)
        \               |                   /
         +-------- kv-core: Command API, project, EDL, undo --------+
                                  |
   Engine (pure Rust, identical on every OS):
     kv-autoedit   kv-audio   kv-gopro   kv-cloud
                                  |
   Platform layer (behind traits, macOS first):
     kv-media (FFmpeg LGPL + VideoToolbox)   kv-render (wgpu/Metal)   kv-cache (SQLite)
```

## Principles

- The auto-edit is a pure, deterministic function: (media analysis, music
  beats, user constraints, seed) -> edit decision list (EDL). No I/O, so it is
  unit- and property-testable in milliseconds.
- Platform code lives behind traits (`MediaBackend`, `Encoder`, `GpuContext`).
  macOS implementations first; others are stubs until needed.
- Every UI action is a `Command` with undo; UI never mutates state directly.
- Heavy work (probe, thumbnails, proxies, analysis) runs on background workers
  with a cache; the UI never blocks.
- The EDL has *sections* from the start (exactly one in the MVP) so that
  multi-track story projects (Roadmap II) do not need a rewrite.

## Planned crates

| Crate | Responsibility | Key dependencies |
| --- | --- | --- |
| kv-core | Project model, timeline, EDL, Command API, undo, project file (JSON) | serde, thiserror |
| kv-autoedit | Segment scoring, beat snapping, constraints (force include/exclude) | proptest (tests) |
| kv-audio | Music decode, onset and tempo detection, beat markers | symphonia, rubato, rustfft |
| kv-gopro | GPMF telemetry, HiLight tags, chaptered file grouping (GH/GX) | none: pure-Rust MP4 box + GPMF KLV reader (ADR 0006) |
| kv-media | Probe, decode, thumbnails, proxies, export | ffmpeg-next (LGPL build), VideoToolbox |
| kv-render | Frame plan (pure) + compositing: transitions, speed, titles, preview frames | wgpu |
| kv-cache | Blob cache (thumbnails, probe, HiLights, music analysis) in one SQLite file, keyed by source path+size+mtime, kind and params; LRU prune | rusqlite (bundled) |
| kv-cli | `kwikvid edit --clips ... --music ... --out ...` | clap |
| kv-mcp | MCP server | rmcp |
| kv-app | Desktop shell and UI | Tauri 2 |
| kv-cloud | Camera (Open GoPro) and cloud import, Roadmap I | reqwest |
| kv-testkit | Fixture generation, golden files, comparisons, regression corpus access (ADR 0008) | hound, toml, sha2, criterion (benches) |

Beat detection is our own implementation: aubio (GPL) and Essentia (AGPL) may
only be used offline as reference tools in the regression suite.
