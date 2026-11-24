//! kwikvid test kit: tiny, deterministic synthetic fixtures generated on demand.
//!
//! Fixtures are rendered in-process (pure Rust, no external tools) and cached
//! under `<workspace>/fixtures/generated/`, which is git-ignored. A fixture that
//! already exists is reused; otherwise it is written atomically (temporary file
//! plus rename), so tests running in parallel processes can share the cache.
//!
//! Generation is deterministic: the same parameters always produce
//! byte-identical files. File names carry a format version, so changing the
//! generator invalidates old cached files instead of silently reusing them.
//!
//! Frames can carry a machine-readable [`FrameTag`] (clip and frame index,
//! see [`render_tagged_frame`] / [`read_frame_tag`]). With the `video` feature,
//! `tagged_clip` and `tagged_clip_set` encode tagged MP4 clips with the system
//! FFmpeg. Those files are the exception to byte-determinism: their bytes
//! depend on the FFmpeg version and encoder, but every frame decodes back to
//! its tag (`read_clip_tags`).
//!
//! This is a test-support crate: functions panic on invalid parameters or I/O
//! failure rather than returning errors, so tests can call them directly.
//!
//! Real camera footage for the full regression suite is listed in
//! `fixtures/corpus.toml` and reached through [`corpus_file`] /
//! [`corpus_file_or_skip!`], which skip cleanly when the corpus is absent.
//!
//! See ADR 0004 (`docs/decisions/0004-test-media.md`) and ADR 0008
//! (`docs/decisions/0008-regression-corpus.md`).

mod audio;
mod cache;
mod corpus;
mod tag;
#[cfg(feature = "video")]
mod video;
mod yuv;

pub use audio::{
    beat_times, click_track, render_click_track_wav, render_tempo_change_wav,
    tempo_change_beat_times, tempo_change_track, TempoSegment, CLICK_DURATION_SECS,
    CLICK_FREQUENCY_HZ, DEFAULT_SAMPLE_RATE, TEMPO_CHANGE_100_140,
};
pub use cache::fixtures_dir;
pub use corpus::{
    corpus_dir, corpus_file, corpus_manifest, corpus_manifest_path, sha256_file, CorpusEntry,
    CorpusManifest,
};
pub use tag::{clip_colour, read_frame_tag, render_tagged_frame, FrameTag, PALETTE_LEN};
#[cfg(feature = "video")]
pub use video::{read_clip_tags, tagged_clip, tagged_clip_set, ClipSpec};
pub use yuv::{rgba_to_yuv420p, yuv420p_to_rgba, Yuv420p};
