//! kwikvid GoPro metadata: HiLight tags and GPMF telemetry streams.
//!
//! A pure-Rust reader (no FFI, no `unsafe`) for the two pieces of GoPro
//! metadata the auto-edit needs first:
//!
//! - [`read_hilights`]: HiLight tags (moments the user marked on the camera),
//!   read from the `moov/udta/HMMT` box.
//! - [`gpmf_streams`]: a summary of the GPMF telemetry track (the `gpmd`
//!   sample entry): one [`StreamInfo`] per sensor FourCC with its total
//!   sample count and average rate.
//!
//! Only the `moov` box and the GPMF samples are read, never the video data,
//! so the functions are cheap on multi-gigabyte files. The approach and the
//! open questions that need real camera footage are recorded in
//! `docs/decisions/0006-gopro-metadata.md`.

mod error;
mod gpmf;
mod mp4;

use std::fs::File;
use std::io::{BufReader, Read, Seek};
use std::path::Path;

use kv_core::Micros;

pub use error::{GoproError, Result};
pub use gpmf::{FourCc, StreamInfo};

/// Read the HiLight tags of a GoPro MP4 file, in ascending order.
///
/// Returns an empty list when the file is a valid MP4 without HiLights.
pub fn read_hilights(path: impl AsRef<Path>) -> Result<Vec<Micros>> {
    read_hilights_from(BufReader::new(File::open(path)?))
}

/// [`read_hilights`] over any seekable reader.
pub fn read_hilights_from<R: Read + Seek>(mut reader: R) -> Result<Vec<Micros>> {
    let moov = mp4::read_moov(&mut reader)?;
    mp4::hilights(&moov)
}

/// Summarise the GPMF telemetry streams of a GoPro MP4 file: one entry per
/// sensor FourCC (e.g. `ACCL`, `GYRO`, `GPS5`), in order of first appearance.
///
/// Returns an empty list when the file has no GPMF (`gpmd`) track.
pub fn gpmf_streams(path: impl AsRef<Path>) -> Result<Vec<StreamInfo>> {
    gpmf_streams_from(BufReader::new(File::open(path)?))
}

/// [`gpmf_streams`] over any seekable reader.
pub fn gpmf_streams_from<R: Read + Seek>(mut reader: R) -> Result<Vec<StreamInfo>> {
    let moov = mp4::read_moov(&mut reader)?;
    let Some(track) = mp4::gpmd_track(&moov)? else {
        return Ok(Vec::new());
    };
    let mut summary = gpmf::Summary::default();
    for (offset, size) in track.samples()? {
        let payload = mp4::read_at(&mut reader, offset, size)?;
        summary.add_payload(&payload)?;
    }
    Ok(summary.finish(track.duration_secs()))
}
