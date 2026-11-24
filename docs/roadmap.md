# Roadmap

| Phase | Delivers | Gate |
| --- | --- | --- |
| M0 Foundation | Repo, CI gate, AGENTS.md, fixtures, kv-core types, Tauri preview spike | Gate green; 60 fps preview proven in Tauri |
| M1 Headless engine | Probe, HiLights, beat analysis, auto-edit, CLI render to MP4, first MCP tools | CLI turns 10 GoPro clips + a song into a beat-synced MP4 on Mac |
| M2 Desktop MVP | Library, timeline, preview, pin/exclude, styles, export, projects | All MVP features pass; owner signs off Quik parity |
| Roadmap I | Speed targets, full MCP surface, camera import, GoPro cloud (opt-in), polish | Perf targets met on M1; MCP drives a full edit |
| Roadmap II | Long projects, multiple music tracks, story sections, auto chapters | 10 min story with 3 tracks exports without manual stitching |

## MVP (Quik parity) features

1. Import MP4/MOV (H.264, HEVC, 10-bit); GoPro chaptered files grouped as one clip.
2. Media library with filmstrips and HiLight markers.
3. Add music: user's own MP3/AAC/WAV/FLAC plus a few bundled CC0 tracks.
4. Local beat analysis (onsets, tempo, tiered cut points), cached.
5. Auto-edit to a target length (15/30/60 s or full song), snapped to beats, deterministic per seed.
6. Force-include a range (always in the edit).
7. Force-exclude a range or clip; re-roll keeps pinned ranges.
8. Manual tweaks: reorder, nudge a cut by one beat, clip audio toggle, music volume.
9. Styles: cut, crossfade, 2x slow-mo by frame blending, one title card; 2-3 presets.
10. Real-time 1080p preview at 30 fps on M1; seek under 150 ms (guess).
11. Export H.264/HEVC via VideoToolbox; 1080p/4K; 16:9, 9:16, 1:1.
12. Save and reopen projects (JSON, relative media paths).

## Performance targets (guesses until measured on an M1)

- Auto-edit regenerate < 300 ms for 50 clips
- Cold start < 1.5 s
- 60 s 1080p export < 20 s
