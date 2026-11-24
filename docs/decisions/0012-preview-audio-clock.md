# 0012 Preview audio: the audio device is the master clock

Status: accepted (2026-11-24)

## Context

A preview without sound cannot judge beat sync (issue #74). The preview
engine (#64) picks frames from a `Clock`; until now that was system time.
Audio and video driven by two clocks drift apart, and the ear notices a
glitch long before the eye notices a dropped frame.

## Decision

- Preview audio lives in `kv-render` (`kv_render::preview::audio`), next to
  the engine and its `Clock` trait; `kv-audio` stays pure analysis and
  rendering.
- The device is behind an `AudioOutput` trait: `CpalOutput` (default output
  device via `cpal`, Apache-2.0) and `NullOutput` (no device).
- `AudioClock` is the engine's clock: frames the device has played (written
  minus the latency the device reports), interpolated with system time
  between callbacks and never going backwards. Without a device it is
  system time.
- The audio callback does not keep its own playhead. It follows the engine's
  `Transport` (playing, position, anchor) and, for every sample, plays the
  bed at the position that will be on screen when the sample is heard. Sync
  holds by construction; seek, play and pause take effect on the next device
  buffer.
- The bed is `render_music_bed` at the device's rate, rendered at 100% with
  the export fade; the music volume is a live gain, so it changes without
  re-rendering and matches export's levels.
- `cpal` is always built on macOS (target-specific dependency) and behind
  the `cpal` feature elsewhere, because on Linux it links ALSA, which the
  merge gate does not install. CI stays unchanged; the `mac-smoke` job
  compiles the device code.

## Consequences

- Hosts create `PreviewAudio` first, give its clock to the engine, then
  `attach` it and call `set_section` after each edit (off the UI thread: it
  decodes the song when the music or length changed).
- Linux and Windows desktop builds need `--features cpal` (Linux also
  `libasound2-dev`) to get sound.
- Clip audio is not mixed yet (TODO(#72)): once export mixes it, the preview
  bed must use the same mix.
