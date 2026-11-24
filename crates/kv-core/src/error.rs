//! Typed errors returned when a [`Command`](crate::Command) is rejected.

use crate::model::{MediaId, RangeRef, SectionId};
use crate::Micros;
use thiserror::Error;

/// Why a command could not be applied. A rejected command never changes the
/// project.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum CoreError {
    #[error("unknown media id {0:?}")]
    UnknownMedia(MediaId),

    #[error("unknown section id {0:?}")]
    UnknownSection(SectionId),

    #[error("media id {0:?} already exists")]
    DuplicateMedia(MediaId),

    #[error("duration must be positive, got {0:?}")]
    InvalidDuration(Micros),

    #[error("path must be non-empty and relative to the project file: {0:?}")]
    InvalidPath(String),

    #[error("hilight at {at:?} is outside the media duration {duration:?}")]
    HilightOutOfRange { at: Micros, duration: Micros },

    #[error("range {start:?}..{end:?} is empty or outside the media duration {duration:?}")]
    InvalidRange {
        start: Micros,
        end: Micros,
        duration: Micros,
    },

    #[error("range {0:?} is already pinned")]
    AlreadyPinned(RangeRef),

    #[error("range {0:?} is already excluded")]
    AlreadyExcluded(RangeRef),

    #[error("range {0:?} is not pinned")]
    PinNotFound(RangeRef),

    #[error("range {0:?} is not excluded")]
    ExcludeNotFound(RangeRef),

    #[error("range {range:?} overlaps the opposite constraint {existing:?}")]
    RangeConflict { range: RangeRef, existing: RangeRef },

    #[error("target length must be positive, got {0:?}")]
    InvalidTargetLength(Micros),

    #[error("segment index {index} is out of range for an EDL of {len} segments")]
    SegmentIndexOutOfRange { index: usize, len: usize },

    #[error("segment {index} is invalid: {reason}")]
    InvalidSegment { index: usize, reason: &'static str },
}
