//! kwikvid core: project model, edit decision list (EDL) and the `Command` API.
//!
//! - [`model`] holds [`Project`], the media library, sections and the
//!   sectioned [`Edl`].
//! - [`command`] holds [`Command`] (every user-facing action), [`Event`] and
//!   [`Project::apply`], which validates and applies a command.
//! - [`history`] holds [`History`], undo/redo over applied commands.
//! - [`error`] holds [`CoreError`].
//!
//! Everything serialises to JSON; a project file has a top-level
//! `"version": 1`. Keep this crate free of I/O and platform code (see
//! `docs/architecture.md`).

pub mod command;
pub mod error;
pub mod history;
pub mod model;

pub use command::{Command, Event};
pub use error::CoreError;
pub use history::History;
pub use model::{
    AspectRatio, Edl, FormatVersion, MediaId, MediaItem, MediaKind, MusicRef, Project, ProjectId,
    ProjectSettings, RangeRef, Section, SectionId, Segment, Speed, StylePreset, TargetLength,
    TitleCard, Transition,
};

use serde::{Deserialize, Serialize};

/// Time on a timeline or within a media file, in microseconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Micros(pub i64);

impl Micros {
    /// Construct from seconds (rounded to the nearest microsecond).
    pub fn from_secs_f64(secs: f64) -> Self {
        Micros((secs * 1_000_000.0).round() as i64)
    }

    /// Value in seconds.
    pub fn as_secs_f64(self) -> f64 {
        self.0 as f64 / 1_000_000.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn micros_round_trip() {
        let t = Micros::from_secs_f64(1.5);
        assert_eq!(t, Micros(1_500_000));
        assert_eq!(t.as_secs_f64(), 1.5);
    }

    #[test]
    fn micros_serde() {
        let json = serde_json::to_string(&Micros(42)).unwrap();
        assert_eq!(json, "42");
    }
}
