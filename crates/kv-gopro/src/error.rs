//! Errors returned by the GoPro metadata reader.

use thiserror::Error;

/// Why GoPro metadata could not be read.
#[derive(Debug, Error)]
pub enum GoproError {
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),

    /// The file has no `moov` box, so it is not a (complete) MP4/MOV file.
    #[error("not an MP4 file: no moov box found")]
    NoMoov,

    /// A box or GPMF entry is truncated or internally inconsistent.
    #[error("malformed {0}")]
    Malformed(String),

    /// A box or sample is larger than this reader is willing to load.
    #[error("{what} is {size} bytes, over the {limit} byte limit")]
    TooLarge {
        what: &'static str,
        size: u64,
        limit: u64,
    },
}

/// Result alias for this crate.
pub type Result<T> = std::result::Result<T, GoproError>;

pub(crate) fn malformed(msg: impl Into<String>) -> GoproError {
    GoproError::Malformed(msg.into())
}
