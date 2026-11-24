//! [`SessionError`]: why a session action failed, with a short message for
//! the UI.

use kv_core::CoreError;
use std::path::PathBuf;
use thiserror::Error;

/// Why a [`Session`](crate::Session) action failed. A failed action never
/// changes the session.
///
/// `Display` is a short, user-facing sentence suitable for a toast. The
/// technical detail stays reachable for logs: through
/// [`std::error::Error::source`] for [`SessionError::Core`], and through the
/// variant fields (`Debug`) otherwise.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum SessionError {
    /// The engine rejected a command.
    #[error("{}", core_message(.0))]
    Core(#[from] CoreError),

    #[error("Nothing to undo.")]
    NothingToUndo,

    #[error("Nothing to redo.")]
    NothingToRedo,

    /// A path given to [`Session::to_project_path`](crate::Session::to_project_path)
    /// is not absolute.
    #[error("That file location is incomplete.")]
    PathNotAbsolute(PathBuf),

    /// The session's media root is not absolute, so no path can be made
    /// relative to it.
    #[error("The project folder location is incomplete.")]
    RootNotAbsolute(PathBuf),

    /// The file is on another drive or network share than the media root, so
    /// no relative path reaches it.
    #[error("Files on another drive than the project can't be added yet.")]
    OtherVolume(PathBuf),

    /// The path is the media root itself, not a file inside or beside it.
    #[error("That is the project folder, not a file.")]
    PathIsRoot(PathBuf),

    /// The path is not valid UTF-8, so it cannot be stored in a project file.
    #[error("That file name contains characters kwikvid can't store.")]
    NonUtf8Path(PathBuf),
}

/// A short, user-facing sentence for a rejected command.
fn core_message(e: &CoreError) -> &'static str {
    match e {
        CoreError::UnknownMedia(_) => "That clip is no longer in the project.",
        CoreError::UnknownSection(_) => "That part of the project no longer exists.",
        CoreError::DuplicateMedia(_) => "That clip is already in the project.",
        CoreError::InvalidDuration(_) => "That file has no playable length.",
        CoreError::InvalidPath(_) => "That file's location can't be stored in the project.",
        CoreError::HilightOutOfRange { .. } => "That clip has a HiLight past its end.",
        CoreError::InvalidRange { .. } => "That range is outside the clip.",
        CoreError::AlreadyPinned(_) => "That moment is already forced in.",
        CoreError::AlreadyExcluded(_) => "That moment is already excluded.",
        CoreError::PinNotFound(_) => "That moment isn't forced in.",
        CoreError::ExcludeNotFound(_) => "That moment isn't excluded.",
        CoreError::RangeConflict { .. } => "That overlaps a moment already forced in or excluded.",
        CoreError::InvalidTargetLength(_) => "The edit length must be longer than zero.",
        CoreError::SegmentIndexOutOfRange { .. } => "That cut is no longer in the timeline.",
        CoreError::InvalidSegment { .. } => "The edit contains an invalid cut.",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::error::Error as _;

    #[test]
    fn core_error_has_short_message_and_keeps_detail_as_source() {
        let core = CoreError::InvalidTargetLength(kv_core::Micros(0));
        let e = SessionError::from(core.clone());
        assert_eq!(e.to_string(), "The edit length must be longer than zero.");
        assert_eq!(e.source().unwrap().to_string(), core.to_string());
    }
}
