//! Synthetic GoPro-style MP4 files, written byte by byte.
//!
//! No openly licensed GoPro sample with HiLights is available, so the tests
//! build the relevant boxes by hand (ADR 0006, "Test fixtures"):
//!
//! ```text
//! ftyp
//! mdat                          GPMF payloads (DEVC > STRM > STNM, ACCL / GYRO)
//! moov
//!   udta
//!     HMMT                      u32 count, count x u32 milliseconds
//!   trak                        video track stub (stsd entry 'avc1'), skipped
//!   trak                        telemetry track
//!     mdia
//!       mdhd                    timescale + duration
//!       hdlr                    'meta'
//!       minf/stbl
//!         stsd                  one 'gpmd' sample entry
//!         stsc, stsz, stco|co64 sample table pointing into mdat
//! ```
//!
//! With `moov_first` the `moov` box comes before `mdat`, as in a
//! fast-start file; chunk offsets are then computed after sizing `moov`.

#![allow(dead_code)]

pub fn bx(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let mut v = ((body.len() + 8) as u32).to_be_bytes().to_vec();
    v.extend_from_slice(kind);
    v.extend_from_slice(body);
    v
}

/// A full box: version/flags followed by `body`.
pub fn full(kind: &[u8; 4], version: u8, body: &[u8]) -> Vec<u8> {
    let mut b = vec![version, 0, 0, 0];
    b.extend_from_slice(body);
    bx(kind, &b)
}

pub fn klv(key: &[u8; 4], typ: u8, size: u8, repeat: u16, data: &[u8]) -> Vec<u8> {
    assert_eq!(data.len(), usize::from(size) * usize::from(repeat));
    let mut v = key.to_vec();
    v.extend_from_slice(&[typ, size]);
    v.extend_from_slice(&repeat.to_be_bytes());
    v.extend_from_slice(data);
    while v.len() % 4 != 0 {
        v.push(0);
    }
    v
}

fn nested(key: &[u8; 4], children: &[u8]) -> Vec<u8> {
    klv(key, 0, 1, children.len() as u16, children)
}

/// One GPMF stream: a name, a scale and `samples` 3-axis `i16` samples.
pub fn strm(key: &[u8; 4], name: &str, samples: u16) -> Vec<u8> {
    let mut s = klv(b"STNM", b'c', 1, name.len() as u16, name.as_bytes());
    s.extend(klv(b"SCAL", b's', 2, 1, &418i16.to_be_bytes()));
    s.extend(klv(
        key,
        b's',
        6,
        samples,
        &vec![0; 6 * usize::from(samples)],
    ));
    nested(b"STRM", &s)
}

/// One GPMF payload (one MP4 sample): a single device with the given streams.
pub fn payload(streams: &[Vec<u8>]) -> Vec<u8> {
    let mut d = klv(b"DVID", b'L', 4, 1, &1u32.to_be_bytes());
    d.extend(klv(b"DVNM", b'c', 1, 6, b"Camera"));
    for s in streams {
        d.extend_from_slice(s);
    }
    nested(b"DEVC", &d)
}

pub struct Spec {
    /// HiLights in milliseconds; `None` writes no `HMMT` box.
    pub hilights_ms: Option<Vec<u32>>,
    /// GPMF payloads, one per MP4 sample; empty writes no telemetry track.
    pub payloads: Vec<Vec<u8>>,
    pub timescale: u32,
    pub duration: u32,
    pub moov_first: bool,
    pub co64: bool,
}

impl Default for Spec {
    fn default() -> Self {
        Spec {
            hilights_ms: Some(vec![12_345, 2_000]),
            payloads: vec![
                payload(&[
                    strm(b"ACCL", "Accelerometer", 200),
                    strm(b"GYRO", "Gyroscope", 200),
                ]),
                payload(&[
                    strm(b"ACCL", "Accelerometer", 198),
                    strm(b"GYRO", "Gyroscope", 200),
                ]),
            ],
            timescale: 1000,
            duration: 2000,
            moov_first: false,
            co64: false,
        }
    }
}

fn telemetry_trak(spec: &Spec, mdat_data_start: u64) -> Vec<u8> {
    let mut mdhd = Vec::new();
    mdhd.extend_from_slice(&0u32.to_be_bytes()); // creation
    mdhd.extend_from_slice(&0u32.to_be_bytes()); // modification
    mdhd.extend_from_slice(&spec.timescale.to_be_bytes());
    mdhd.extend_from_slice(&spec.duration.to_be_bytes());
    mdhd.extend_from_slice(&[0x55, 0xc4, 0, 0]); // language, pre_defined
    let mdhd = full(b"mdhd", 0, &mdhd);

    let mut hdlr = 0u32.to_be_bytes().to_vec();
    hdlr.extend_from_slice(b"meta");
    hdlr.extend_from_slice(&[0; 12]);
    hdlr.extend_from_slice(b"GoPro MET\0");
    let hdlr = full(b"hdlr", 0, &hdlr);

    // gpmd sample entry: 6 reserved bytes + data_reference_index.
    let gpmd = bx(b"gpmd", &[0, 0, 0, 0, 0, 0, 0, 1]);
    let mut stsd = 1u32.to_be_bytes().to_vec();
    stsd.extend(gpmd);
    let stsd = full(b"stsd", 0, &stsd);

    // One sample per chunk.
    let n = spec.payloads.len() as u32;
    let mut stsc = 1u32.to_be_bytes().to_vec();
    stsc.extend_from_slice(&1u32.to_be_bytes());
    stsc.extend_from_slice(&1u32.to_be_bytes());
    stsc.extend_from_slice(&1u32.to_be_bytes());
    let stsc = full(b"stsc", 0, &stsc);

    let mut stsz = 0u32.to_be_bytes().to_vec();
    stsz.extend_from_slice(&n.to_be_bytes());
    for p in &spec.payloads {
        stsz.extend_from_slice(&(p.len() as u32).to_be_bytes());
    }
    let stsz = full(b"stsz", 0, &stsz);

    let mut offsets = Vec::new();
    let mut at = mdat_data_start;
    for p in &spec.payloads {
        offsets.push(at);
        at += p.len() as u64;
    }
    let mut co = n.to_be_bytes().to_vec();
    for o in &offsets {
        if spec.co64 {
            co.extend_from_slice(&o.to_be_bytes());
        } else {
            co.extend_from_slice(&(*o as u32).to_be_bytes());
        }
    }
    let co = full(if spec.co64 { b"co64" } else { b"stco" }, 0, &co);

    let stbl = bx(b"stbl", &[stsd, stsc, stsz, co].concat());
    let minf = bx(b"minf", &stbl);
    bx(b"trak", &bx(b"mdia", &[mdhd, hdlr, minf].concat()))
}

fn video_trak() -> Vec<u8> {
    let mut stsd = 1u32.to_be_bytes().to_vec();
    stsd.extend(bx(b"avc1", &[0; 78]));
    let stbl = bx(b"stbl", &full(b"stsd", 0, &stsd));
    bx(b"trak", &bx(b"mdia", &bx(b"minf", &stbl)))
}

fn moov(spec: &Spec, mdat_data_start: u64) -> Vec<u8> {
    let mut body = full(b"mvhd", 0, &[0; 96]);
    if let Some(ms) = &spec.hilights_ms {
        let mut hmmt = (ms.len() as u32).to_be_bytes().to_vec();
        for m in ms {
            hmmt.extend_from_slice(&m.to_be_bytes());
        }
        body.extend(bx(b"udta", &bx(b"HMMT", &hmmt)));
    }
    body.extend(video_trak());
    if !spec.payloads.is_empty() {
        body.extend(telemetry_trak(spec, mdat_data_start));
    }
    bx(b"moov", &body)
}

pub fn build(spec: &Spec) -> Vec<u8> {
    let ftyp = bx(b"ftyp", b"mp41\0\0\0\0mp41");
    let mdat = bx(b"mdat", &spec.payloads.concat());
    if spec.moov_first {
        // Offsets do not change the size of moov, so size it first.
        let moov_len = moov(spec, 0).len() as u64;
        let start = ftyp.len() as u64 + moov_len + 8;
        [ftyp, moov(spec, start), mdat].concat()
    } else {
        let start = (ftyp.len() + 8) as u64;
        let m = moov(spec, start);
        [ftyp, mdat, m].concat()
    }
}

/// Write `bytes` to a fresh file under Cargo's per-test temp dir.
pub fn write_temp(name: &str, bytes: &[u8]) -> std::path::PathBuf {
    let path = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join(name);
    std::fs::write(&path, bytes).unwrap();
    path
}
