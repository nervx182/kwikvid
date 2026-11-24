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
//! This is a test-support crate: functions panic on invalid parameters or I/O
//! failure rather than returning errors, so tests can call them directly.
//!
//! See ADR 0004 (`docs/decisions/0004-test-media.md`).

mod audio;
mod cache;

pub use audio::{
    beat_times, click_track, render_click_track_wav, render_tempo_change_wav,
    tempo_change_beat_times, tempo_change_track, TempoSegment, CLICK_DURATION_SECS,
    CLICK_FREQUENCY_HZ, DEFAULT_SAMPLE_RATE, TEMPO_CHANGE_100_140,
};
pub use cache::fixtures_dir;
