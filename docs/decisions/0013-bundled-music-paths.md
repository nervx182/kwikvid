# 0013 Bundled music: referenced in place from the app resources

Status: accepted (2026-11-25), reversible

## Context

A section's music is a `MusicRef` whose `path` is a project path:
`/`-separated, relative to the media root (the project file's folder, or the
home folder for an untitled project), with `..` steps for files outside it
(`kv-session`, `paths.rs`). The music picker (#66) also offers the bundled
CC0 tracks (#48, #67), which ship as app resources: `assets/music/` is copied
to the `music/` resource folder (`bundle.resources` in
`crates/kv-app/tauri.conf.json`), next to `manifest.toml`. On macOS that is
`kwikvid.app/Contents/Resources/music/`; in a dev build, `target/<profile>/music/`.

Options: (1) copy the bundled file into the project folder when the project
is saved, so the project is self-contained; (2) reference the file where the
app installed it, like any other song.

## Decision

Option 2. Picking a bundled track sends its absolute resource path to
`analyze_music`, exactly like a file from the picker. The session turns it
into an ordinary project path, so an untitled project in `/Users/ana` refers
to `../../Applications/kwikvid.app/Contents/Resources/music/arpent.mp3`, and
Save As rebases it to the project folder like every other path. No new path
kind, no copy step, no change to the project file format.

## Consequences

- Nothing new to save, and no duplicate copies of the same track in every
  project.
- The project only finds the track while the app is installed at the same
  place. Moving the app, or opening the project on a machine where it lives
  elsewhere, reports the song as missing (the project-open missing-files
  report). On Windows a project on another drive than the app cannot
  reference the track at all: `analyze_music` refuses with a readable
  message, because a project path cannot cross drives.
- Reversing it (option 1) is local to the save path: copy any music whose
  resolved path is under the resource folder into the project folder and
  rewrite its `MusicRef.path`; projects written under this decision keep
  working, since their paths are still valid relative paths.
