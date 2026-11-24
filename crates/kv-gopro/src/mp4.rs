//! Minimal ISO-BMFF (MP4/MOV) box walker: just enough to find `moov`, the
//! `udta/HMMT` HiLight box and the sample table of the GPMF (`gpmd`) track.
//!
//! Box layout (ISO/IEC 14496-12 §4.2): a big-endian `u32` size, a FourCC
//! type, then the body. Size 1 means a `u64` "largesize" follows the type;
//! size 0 means the box runs to the end of its container.

use std::io::{ErrorKind, Read, Seek, SeekFrom};

use kv_core::Micros;

use crate::error::{malformed, GoproError, Result};

/// Largest `moov` box we load into memory. Real GoPro `moov` boxes are a few
/// MB even for long recordings (guess); this only guards against corrupt
/// sizes.
pub(crate) const MAX_MOOV_BYTES: u64 = 256 * 1024 * 1024;

/// Largest single GPMF sample we load. GoPro writes roughly one payload per
/// second, a few KB each (guess).
pub(crate) const MAX_SAMPLE_BYTES: u64 = 16 * 1024 * 1024;

/// Most samples accepted from an `stsz` box with a fixed sample size.
const MAX_FIXED_SIZE_SAMPLES: usize = 1 << 24;

/// One box inside an in-memory buffer.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Mp4Box<'a> {
    pub kind: [u8; 4],
    pub body: &'a [u8],
}

/// Iterate the boxes directly inside `data`.
pub(crate) fn boxes(data: &[u8]) -> Boxes<'_> {
    Boxes { rest: data }
}

pub(crate) struct Boxes<'a> {
    rest: &'a [u8],
}

impl<'a> Iterator for Boxes<'a> {
    type Item = Result<Mp4Box<'a>>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.rest.is_empty() {
            return None;
        }
        let result = split_box(self.rest);
        match result {
            Ok((b, rest)) => {
                self.rest = rest;
                Some(Ok(b))
            }
            Err(e) => {
                self.rest = &[];
                Some(Err(e))
            }
        }
    }
}

fn split_box(data: &[u8]) -> Result<(Mp4Box<'_>, &[u8])> {
    if data.len() < 8 {
        return Err(malformed("box header: fewer than 8 bytes left"));
    }
    let size32 = be_u32(data, 0)?;
    let kind = fourcc(data, 4)?;
    let (header, size) = match size32 {
        0 => (8, data.len() as u64),
        1 => (16, be_u64(data, 8)?),
        n => (8, u64::from(n)),
    };
    if size < header as u64 || size > data.len() as u64 {
        return Err(malformed(format!(
            "box '{}': size {size} does not fit its container ({} bytes left)",
            String::from_utf8_lossy(&kind),
            data.len()
        )));
    }
    let size = size as usize;
    Ok((
        Mp4Box {
            kind,
            body: &data[header..size],
        },
        &data[size..],
    ))
}

/// The first child of `data` with type `kind`.
pub(crate) fn find<'a>(data: &'a [u8], kind: &[u8; 4]) -> Result<Option<&'a [u8]>> {
    for b in boxes(data) {
        let b = b?;
        if &b.kind == kind {
            return Ok(Some(b.body));
        }
    }
    Ok(None)
}

/// Follow a path of box types from `data`, taking the first match at each level.
pub(crate) fn find_path<'a>(data: &'a [u8], path: &[&[u8; 4]]) -> Result<Option<&'a [u8]>> {
    let mut cur = data;
    for kind in path {
        match find(cur, kind)? {
            Some(body) => cur = body,
            None => return Ok(None),
        }
    }
    Ok(Some(cur))
}

/// Walk the top-level boxes of a file and return the body of `moov`.
/// Skips over `mdat` and everything else by seeking, so the video data is
/// never read.
pub(crate) fn read_moov<R: Read + Seek>(r: &mut R) -> Result<Vec<u8>> {
    let file_len = r.seek(SeekFrom::End(0))?;
    let mut pos = r.seek(SeekFrom::Start(0))?;
    while pos < file_len {
        let mut head = [0u8; 8];
        read_exact_or_malformed(r, &mut head, "top-level box header")?;
        let size32 = u32::from_be_bytes([head[0], head[1], head[2], head[3]]);
        let kind = [head[4], head[5], head[6], head[7]];
        let (header, size) = match size32 {
            0 => (8u64, file_len - pos),
            1 => {
                let mut large = [0u8; 8];
                read_exact_or_malformed(r, &mut large, "top-level box largesize")?;
                (16, u64::from_be_bytes(large))
            }
            n => (8, u64::from(n)),
        };
        if size < header || size > file_len - pos {
            return Err(malformed(format!(
                "top-level box '{}' at offset {pos}: size {size} does not fit the file",
                String::from_utf8_lossy(&kind)
            )));
        }
        if &kind == b"moov" {
            let body_len = size - header;
            if body_len > MAX_MOOV_BYTES {
                return Err(GoproError::TooLarge {
                    what: "moov box",
                    size: body_len,
                    limit: MAX_MOOV_BYTES,
                });
            }
            let mut body = vec![0u8; body_len as usize];
            read_exact_or_malformed(r, &mut body, "moov box")?;
            return Ok(body);
        }
        pos += size;
        r.seek(SeekFrom::Start(pos))?;
    }
    Err(GoproError::NoMoov)
}

/// Read `size` bytes at absolute `offset`.
pub(crate) fn read_at<R: Read + Seek>(r: &mut R, offset: u64, size: u64) -> Result<Vec<u8>> {
    if size > MAX_SAMPLE_BYTES {
        return Err(GoproError::TooLarge {
            what: "GPMF sample",
            size,
            limit: MAX_SAMPLE_BYTES,
        });
    }
    r.seek(SeekFrom::Start(offset))?;
    let mut buf = vec![0u8; size as usize];
    read_exact_or_malformed(r, &mut buf, "GPMF sample (points past end of file)")?;
    Ok(buf)
}

fn read_exact_or_malformed<R: Read>(r: &mut R, buf: &mut [u8], what: &str) -> Result<()> {
    r.read_exact(buf).map_err(|e| match e.kind() {
        ErrorKind::UnexpectedEof => malformed(format!("{what}: truncated")),
        _ => GoproError::Io(e),
    })
}

/// HiLights from `moov/udta/HMMT`: a big-endian `u32` count followed by that
/// many `u32` millisecond offsets from the start of the file. This layout is
/// the one VLC's MP4 demuxer reads (see ADR 0006). Returned sorted.
pub(crate) fn hilights(moov: &[u8]) -> Result<Vec<Micros>> {
    let Some(hmmt) = find_path(moov, &[b"udta", b"HMMT"])? else {
        return Ok(Vec::new());
    };
    let count = be_u32(hmmt, 0).map_err(|_| malformed("HMMT box: missing count"))? as usize;
    let needed = count
        .checked_mul(4)
        .and_then(|n| n.checked_add(4))
        .ok_or_else(|| malformed("HMMT box: count overflows"))?;
    if hmmt.len() < needed {
        return Err(malformed(format!(
            "HMMT box: count {count} needs {needed} bytes, box has {}",
            hmmt.len()
        )));
    }
    let mut out: Vec<Micros> = (0..count)
        .map(|i| be_u32(hmmt, 4 + 4 * i).map(|ms| Micros(i64::from(ms) * 1000)))
        .collect::<Result<_>>()?;
    out.sort();
    Ok(out)
}

/// The sample table of the GPMF telemetry track.
#[derive(Debug)]
pub(crate) struct GpmdTrack {
    timescale: u32,
    duration: u64,
    sizes: Vec<u64>,
    chunk_offsets: Vec<u64>,
    /// `stsc` entries: (first_chunk, samples_per_chunk), 1-based chunks.
    stsc: Vec<(u32, u32)>,
}

impl GpmdTrack {
    pub(crate) fn duration_secs(&self) -> f64 {
        if self.timescale == 0 {
            0.0
        } else {
            self.duration as f64 / f64::from(self.timescale)
        }
    }

    /// (file offset, size) of every sample, in decode order.
    pub(crate) fn samples(&self) -> Result<Vec<(u64, u64)>> {
        let mut out = Vec::with_capacity(self.sizes.len());
        let mut sizes = self.sizes.iter();
        for (chunk_idx, &chunk_offset) in self.chunk_offsets.iter().enumerate() {
            let chunk_no = chunk_idx as u64 + 1;
            let per_chunk = self
                .stsc
                .iter()
                .rev()
                .find(|(first, _)| u64::from(*first) <= chunk_no)
                .map(|&(_, n)| n)
                .ok_or_else(|| malformed("stsc box: no entry covers chunk 1"))?;
            let mut offset = chunk_offset;
            for _ in 0..per_chunk {
                let Some(&size) = sizes.next() else {
                    return Ok(out);
                };
                out.push((offset, size));
                offset = offset
                    .checked_add(size)
                    .ok_or_else(|| malformed("sample offset overflows"))?;
            }
        }
        if sizes.next().is_some() {
            return Err(malformed("stsz box: more samples than the chunks hold"));
        }
        Ok(out)
    }
}

/// Find the first track whose sample description is `gpmd` (GoPro's GPMF
/// sample entry) and read its sample table.
pub(crate) fn gpmd_track(moov: &[u8]) -> Result<Option<GpmdTrack>> {
    for b in boxes(moov) {
        let b = b?;
        if &b.kind != b"trak" {
            continue;
        }
        let Some(mdia) = find(b.body, b"mdia")? else {
            continue;
        };
        let Some(stbl) = find_path(mdia, &[b"minf", b"stbl"])? else {
            continue;
        };
        let Some(stsd) = find(stbl, b"stsd")? else {
            continue;
        };
        // stsd: version/flags (4), entry_count (4), then sample entry boxes.
        let entries = stsd
            .get(8..)
            .ok_or_else(|| malformed("stsd box: truncated"))?;
        let is_gpmd = match boxes(entries).next() {
            Some(entry) => &entry?.kind == b"gpmd",
            None => false,
        };
        if !is_gpmd {
            continue;
        }
        let mdhd = find(mdia, b"mdhd")?.ok_or_else(|| malformed("gpmd track: no mdhd box"))?;
        let (timescale, duration) = parse_mdhd(mdhd)?;
        let stsz = find(stbl, b"stsz")?.ok_or_else(|| malformed("gpmd track: no stsz box"))?;
        let stsc = find(stbl, b"stsc")?.ok_or_else(|| malformed("gpmd track: no stsc box"))?;
        let chunk_offsets = if let Some(stco) = find(stbl, b"stco")? {
            parse_table(stco, 4, "stco", |d, o| be_u32(d, o).map(u64::from))?
        } else if let Some(co64) = find(stbl, b"co64")? {
            parse_table(co64, 8, "co64", be_u64)?
        } else {
            return Err(malformed("gpmd track: no stco or co64 box"));
        };
        return Ok(Some(GpmdTrack {
            timescale,
            duration,
            sizes: parse_stsz(stsz)?,
            chunk_offsets,
            stsc: parse_table(stsc, 12, "stsc", |d, o| {
                Ok((be_u32(d, o)?, be_u32(d, o + 4)?))
            })?,
        }));
    }
    Ok(None)
}

fn parse_mdhd(d: &[u8]) -> Result<(u32, u64)> {
    let version = *d.first().ok_or_else(|| malformed("mdhd box: empty"))?;
    let parsed = if version == 1 {
        // v/f 4, creation 8, modification 8, timescale 4, duration 8.
        be_u32(d, 20).and_then(|ts| Ok((ts, be_u64(d, 24)?)))
    } else {
        // v/f 4, creation 4, modification 4, timescale 4, duration 4.
        be_u32(d, 12).and_then(|ts| Ok((ts, u64::from(be_u32(d, 16)?))))
    };
    parsed.map_err(|_| malformed("mdhd box: truncated"))
}

fn parse_stsz(d: &[u8]) -> Result<Vec<u64>> {
    // v/f 4, sample_size 4, sample_count 4, then sizes when sample_size == 0.
    let fixed = be_u32(d, 4).map_err(|_| malformed("stsz box: truncated"))?;
    let count = be_u32(d, 8).map_err(|_| malformed("stsz box: truncated"))? as usize;
    if fixed != 0 {
        // Cap the allocation: a fixed size gives no table to check count
        // against. GoPro writes about one GPMF sample per second (guess), so
        // 2^24 samples is months of telemetry.
        if count > MAX_FIXED_SIZE_SAMPLES {
            return Err(malformed("stsz box: implausible sample count"));
        }
        return Ok(vec![u64::from(fixed); count]);
    }
    let table = &d[12..];
    if table.len() / 4 < count {
        return Err(malformed("stsz box: fewer entries than sample_count"));
    }
    (0..count)
        .map(|i| be_u32(table, 4 * i).map(u64::from))
        .collect()
}

/// Parse a full box holding `version/flags`, a `u32` entry count and then
/// fixed-width entries.
fn parse_table<T>(
    d: &[u8],
    width: usize,
    name: &str,
    entry: impl Fn(&[u8], usize) -> Result<T>,
) -> Result<Vec<T>> {
    let count = be_u32(d, 4).map_err(|_| malformed(format!("{name} box: truncated")))? as usize;
    let table = &d[8..];
    if table.len() / width < count {
        return Err(malformed(format!("{name} box: fewer entries than count")));
    }
    (0..count).map(|i| entry(table, i * width)).collect()
}

pub(crate) fn be_u32(d: &[u8], at: usize) -> Result<u32> {
    d.get(at..at + 4)
        .map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
        .ok_or_else(|| malformed("read past end of box"))
}

pub(crate) fn be_u64(d: &[u8], at: usize) -> Result<u64> {
    d.get(at..at + 8)
        .map(|b| u64::from_be_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]]))
        .ok_or_else(|| malformed("read past end of box"))
}

fn fourcc(d: &[u8], at: usize) -> Result<[u8; 4]> {
    d.get(at..at + 4)
        .map(|b| [b[0], b[1], b[2], b[3]])
        .ok_or_else(|| malformed("read past end of box"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bx(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut v = ((body.len() + 8) as u32).to_be_bytes().to_vec();
        v.extend_from_slice(kind);
        v.extend_from_slice(body);
        v
    }

    #[test]
    fn walks_siblings_and_largesize_and_to_end() {
        let mut data = bx(b"free", b"abc");
        // largesize box: size 1, type, u64 size (16 header + 2 body).
        data.extend_from_slice(&1u32.to_be_bytes());
        data.extend_from_slice(b"skip");
        data.extend_from_slice(&18u64.to_be_bytes());
        data.extend_from_slice(b"xy");
        // size 0: runs to the end.
        data.extend_from_slice(&0u32.to_be_bytes());
        data.extend_from_slice(b"last");
        data.extend_from_slice(b"tail");
        let got: Vec<_> = boxes(&data).collect::<Result<_>>().unwrap();
        assert_eq!(got.len(), 3);
        assert_eq!(&got[0].kind, b"free");
        assert_eq!(got[0].body, b"abc");
        assert_eq!(&got[1].kind, b"skip");
        assert_eq!(got[1].body, b"xy");
        assert_eq!(&got[2].kind, b"last");
        assert_eq!(got[2].body, b"tail");
    }

    #[test]
    fn oversized_box_is_malformed() {
        let mut data = bx(b"free", b"abc");
        data.truncate(9);
        let err = boxes(&data).next().unwrap().unwrap_err();
        assert!(matches!(err, GoproError::Malformed(_)), "{err}");
    }

    #[test]
    fn hmmt_ms_to_micros_sorted() {
        let mut body = 2u32.to_be_bytes().to_vec();
        body.extend_from_slice(&4500u32.to_be_bytes());
        body.extend_from_slice(&1200u32.to_be_bytes());
        let moov = bx(b"udta", &bx(b"HMMT", &body));
        assert_eq!(
            hilights(&moov).unwrap(),
            vec![Micros(1_200_000), Micros(4_500_000)]
        );
    }

    #[test]
    fn hmmt_count_larger_than_box_is_malformed() {
        let mut body = 3u32.to_be_bytes().to_vec();
        body.extend_from_slice(&1000u32.to_be_bytes());
        let moov = bx(b"udta", &bx(b"HMMT", &body));
        assert!(matches!(hilights(&moov), Err(GoproError::Malformed(_))));
    }

    #[test]
    fn no_udta_means_no_hilights() {
        assert!(hilights(&bx(b"mvhd", &[0; 100])).unwrap().is_empty());
    }

    #[test]
    fn mdhd_versions() {
        let mut v0 = vec![0u8; 24];
        v0[12..16].copy_from_slice(&1000u32.to_be_bytes());
        v0[16..20].copy_from_slice(&5000u32.to_be_bytes());
        assert_eq!(parse_mdhd(&v0).unwrap(), (1000, 5000));
        let mut v1 = vec![0u8; 36];
        v1[0] = 1;
        v1[20..24].copy_from_slice(&90000u32.to_be_bytes());
        v1[24..32].copy_from_slice(&(1u64 << 40).to_be_bytes());
        assert_eq!(parse_mdhd(&v1).unwrap(), (90000, 1 << 40));
    }

    #[test]
    fn samples_follow_stsc_runs() {
        let t = GpmdTrack {
            timescale: 1000,
            duration: 3000,
            sizes: vec![10, 20, 30, 40, 50],
            chunk_offsets: vec![100, 500, 900],
            // chunks 1-2 hold 2 samples each, chunk 3 holds 1.
            stsc: vec![(1, 2), (3, 1)],
        };
        assert_eq!(
            t.samples().unwrap(),
            vec![(100, 10), (110, 20), (500, 30), (530, 40), (900, 50)]
        );
    }
}
