//! GPMF (GoPro Metadata Format) KLV parsing and stream summaries.
//!
//! Every GPMF entry is a key-length-value triple, big-endian and 32-bit
//! aligned (gpmf-parser README):
//!
//! ```text
//! key: FourCC (4) | type (1) | struct size (1) | repeat (u16) | data, padded to 4
//! ```
//!
//! The data length is `struct size * repeat`. Type 0 means the data is itself
//! a list of KLVs (nesting). A telemetry payload is a list of `DEVC` (device)
//! entries, each holding `STRM` (stream) entries. Inside a `STRM`, sticky
//! metadata (`STNM` name, `SIUN` units, `SCAL` scale, ...) comes first and the
//! last entry is the sensor data itself, whose `repeat` is the number of
//! samples in this payload.

use std::fmt;

use crate::error::{malformed, Result};

/// A GPMF key, e.g. `ACCL`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct FourCc(pub [u8; 4]);

impl fmt::Display for FourCc {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&String::from_utf8_lossy(&self.0))
    }
}

/// Summary of one telemetry stream across the whole file.
#[derive(Debug, Clone, PartialEq)]
pub struct StreamInfo {
    /// The sensor data key, e.g. `ACCL`, `GYRO`, `GPS5`.
    pub fourcc: FourCc,
    /// The stream's `STNM` name, when the camera writes one.
    pub name: Option<String>,
    /// Total samples across all payloads.
    pub samples: u64,
    /// Average sample rate in Hz: `samples` over the telemetry track
    /// duration. 0 when the track duration is 0.
    pub rate_hz: f64,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct Klv<'a> {
    pub key: [u8; 4],
    pub typ: u8,
    pub repeat: u16,
    pub data: &'a [u8],
}

impl Klv<'_> {
    fn is_nested(&self) -> bool {
        self.typ == 0
    }
}

/// Iterate the KLVs directly inside `data`. Stops at the end of the buffer
/// or at an all-zero key (trailing padding).
pub(crate) fn klvs(data: &[u8]) -> Klvs<'_> {
    Klvs { rest: data }
}

pub(crate) struct Klvs<'a> {
    rest: &'a [u8],
}

impl<'a> Iterator for Klvs<'a> {
    type Item = Result<Klv<'a>>;

    fn next(&mut self) -> Option<Self::Item> {
        let d = self.rest;
        if d.len() < 8 || d[..4] == [0; 4] {
            self.rest = &[];
            let padding = d.len() >= 4 && d[..4] == [0; 4] || d.iter().all(|&b| b == 0);
            return (!padding).then(|| Err(malformed("GPMF entry: header truncated")));
        }
        let key = [d[0], d[1], d[2], d[3]];
        let typ = d[4];
        let size = usize::from(d[5]);
        let repeat = u16::from_be_bytes([d[6], d[7]]);
        let len = size * usize::from(repeat);
        let padded = (len + 3) & !3;
        let body = &d[8..];
        if body.len() < len {
            self.rest = &[];
            return Some(Err(malformed(format!(
                "GPMF entry '{}': {len} data bytes, {} left",
                FourCc(key),
                body.len()
            ))));
        }
        self.rest = &body[padded.min(body.len())..];
        Some(Ok(Klv {
            key,
            typ,
            repeat,
            data: &body[..len],
        }))
    }
}

/// Accumulates per-stream sample counts over the payloads of a track.
#[derive(Debug, Default)]
pub(crate) struct Summary {
    streams: Vec<(FourCc, Option<String>, u64)>,
}

impl Summary {
    pub(crate) fn add_payload(&mut self, payload: &[u8]) -> Result<()> {
        for devc in klvs(payload) {
            let devc = devc?;
            if &devc.key != b"DEVC" || !devc.is_nested() {
                continue;
            }
            for strm in klvs(devc.data) {
                let strm = strm?;
                if &strm.key != b"STRM" || !strm.is_nested() {
                    continue;
                }
                self.add_stream(strm.data)?;
            }
        }
        Ok(())
    }

    fn add_stream(&mut self, strm: &[u8]) -> Result<()> {
        let mut name = None;
        let mut data = None;
        for klv in klvs(strm) {
            let klv = klv?;
            if klv.is_nested() {
                continue;
            }
            if &klv.key == b"STNM" {
                let text = klv.data.split(|&b| b == 0).next().unwrap_or_default();
                name = Some(String::from_utf8_lossy(text).into_owned());
            }
            data = Some(klv);
        }
        let Some(data) = data else {
            return Ok(());
        };
        let fourcc = FourCc(data.key);
        let samples = u64::from(data.repeat);
        match self.streams.iter_mut().find(|(k, _, _)| *k == fourcc) {
            Some(entry) => {
                entry.2 += samples;
                if entry.1.is_none() {
                    entry.1 = name;
                }
            }
            None => self.streams.push((fourcc, name, samples)),
        }
        Ok(())
    }

    pub(crate) fn finish(self, duration_secs: f64) -> Vec<StreamInfo> {
        self.streams
            .into_iter()
            .map(|(fourcc, name, samples)| StreamInfo {
                fourcc,
                name,
                samples,
                rate_hz: if duration_secs > 0.0 {
                    samples as f64 / duration_secs
                } else {
                    0.0
                },
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn klv(key: &[u8; 4], typ: u8, size: u8, repeat: u16, data: &[u8]) -> Vec<u8> {
        let mut v = key.to_vec();
        v.extend_from_slice(&[typ, size]);
        v.extend_from_slice(&repeat.to_be_bytes());
        v.extend_from_slice(data);
        while v.len() % 4 != 0 {
            v.push(0);
        }
        v
    }

    #[test]
    fn padding_is_skipped() {
        // 3 data bytes padded to 4, then a second entry.
        let mut d = klv(b"STNM", b'c', 1, 3, b"abc");
        d.extend(klv(b"SIUN", b'c', 1, 1, b"g"));
        let got: Vec<_> = klvs(&d).collect::<Result<_>>().unwrap();
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].data, b"abc");
        assert_eq!(&got[1].key, b"SIUN");
    }

    #[test]
    fn truncated_data_is_malformed() {
        let mut d = klv(b"ACCL", b's', 6, 2, &[0; 12]);
        d.truncate(14);
        assert!(klvs(&d).next().unwrap().is_err());
    }

    #[test]
    fn trailing_zero_padding_ends_iteration() {
        let mut d = klv(b"TICK", b'L', 4, 1, &[0, 0, 0, 1]);
        d.extend_from_slice(&[0; 12]);
        assert_eq!(klvs(&d).count(), 1);
    }

    #[test]
    fn last_entry_in_strm_is_the_data_and_counts_add_up() {
        let strm = |n: u16| {
            let mut s = klv(b"STNM", b'c', 1, 5, b"Accel");
            s.extend(klv(b"SCAL", b's', 2, 1, &[0, 100]));
            s.extend(klv(b"ACCL", b's', 6, n, &vec![0; 6 * usize::from(n)]));
            klv(b"STRM", 0, 1, s.len() as u16, &s)
        };
        let devc = |n| {
            let mut d = klv(b"DVID", b'L', 4, 1, &[0, 0, 0, 1]);
            d.extend(strm(n));
            klv(b"DEVC", 0, 1, d.len() as u16, &d)
        };
        let mut summary = Summary::default();
        summary.add_payload(&devc(200)).unwrap();
        summary.add_payload(&devc(198)).unwrap();
        let got = summary.finish(2.0);
        assert_eq!(
            got,
            vec![StreamInfo {
                fourcc: FourCc(*b"ACCL"),
                name: Some("Accel".into()),
                samples: 398,
                rate_hz: 199.0,
            }]
        );
    }
}
