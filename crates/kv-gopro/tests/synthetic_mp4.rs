//! End-to-end tests on synthetic GoPro-style MP4 files (see `common/mod.rs`).

mod common;

use std::io::Cursor;

use common::{build, payload, strm, write_temp, Spec};
use kv_core::Micros;
use kv_gopro::{
    gpmf_streams, gpmf_streams_from, read_hilights, read_hilights_from, FourCc, GoproError,
    StreamInfo,
};
use proptest::prelude::*;

#[test]
fn reads_hilights_from_file_sorted_in_micros() {
    let path = write_temp("hilights.mp4", &build(&Spec::default()));
    assert_eq!(
        read_hilights(&path).unwrap(),
        vec![Micros(2_000_000), Micros(12_345_000)]
    );
}

#[test]
fn summarises_gpmf_streams_from_file() {
    let path = write_temp("streams.mp4", &build(&Spec::default()));
    assert_eq!(
        gpmf_streams(&path).unwrap(),
        vec![
            StreamInfo {
                fourcc: FourCc(*b"ACCL"),
                name: Some("Accelerometer".into()),
                samples: 398,
                rate_hz: 199.0,
            },
            StreamInfo {
                fourcc: FourCc(*b"GYRO"),
                name: Some("Gyroscope".into()),
                samples: 400,
                rate_hz: 200.0,
            },
        ]
    );
}

#[test]
fn fast_start_layout_with_co64() {
    let spec = Spec {
        moov_first: true,
        co64: true,
        timescale: 90_000,
        duration: 180_000,
        ..Spec::default()
    };
    let bytes = build(&spec);
    let streams = gpmf_streams_from(Cursor::new(&bytes)).unwrap();
    assert_eq!(streams.len(), 2);
    assert_eq!(streams[0].samples, 398);
    assert_eq!(streams[1].rate_hz, 200.0);
    assert_eq!(read_hilights_from(Cursor::new(&bytes)).unwrap().len(), 2);
}

#[test]
fn streams_appearing_later_are_added_in_order() {
    let spec = Spec {
        payloads: vec![
            payload(&[strm(b"ACCL", "Accelerometer", 10)]),
            payload(&[strm(b"GPS5", "GPS", 18), strm(b"ACCL", "Accelerometer", 10)]),
        ],
        ..Spec::default()
    };
    let streams = gpmf_streams_from(Cursor::new(build(&spec))).unwrap();
    let keys: Vec<_> = streams
        .iter()
        .map(|s| (s.fourcc.to_string(), s.samples))
        .collect();
    assert_eq!(keys, vec![("ACCL".into(), 20), ("GPS5".into(), 18)]);
}

#[test]
fn valid_mp4_without_gopro_metadata_is_empty() {
    let spec = Spec {
        hilights_ms: None,
        payloads: Vec::new(),
        ..Spec::default()
    };
    let bytes = build(&spec);
    assert!(read_hilights_from(Cursor::new(&bytes)).unwrap().is_empty());
    assert!(gpmf_streams_from(Cursor::new(&bytes)).unwrap().is_empty());
}

#[test]
fn zero_duration_gives_zero_rate() {
    let spec = Spec {
        duration: 0,
        ..Spec::default()
    };
    let streams = gpmf_streams_from(Cursor::new(build(&spec))).unwrap();
    assert_eq!(streams[0].rate_hz, 0.0);
}

#[test]
fn file_without_moov_is_rejected() {
    let ftyp_only = common::bx(b"ftyp", b"mp41\0\0\0\0mp41");
    assert!(matches!(
        read_hilights_from(Cursor::new(ftyp_only)),
        Err(GoproError::NoMoov)
    ));
    let text = b"this is not an mp4 file at all".to_vec();
    assert!(read_hilights_from(Cursor::new(text)).is_err());
}

#[test]
fn sample_offsets_past_end_of_file_are_malformed() {
    let mut bytes = build(&Spec {
        moov_first: true,
        ..Spec::default()
    });
    // Drop the tail of mdat: the sample table now points past the end.
    bytes.truncate(bytes.len() - 16);
    // moov comes before mdat, so HiLights still read fine...
    assert_eq!(read_hilights_from(Cursor::new(&bytes)).unwrap().len(), 2);
    // ...but the telemetry samples cannot be read.
    assert!(matches!(
        gpmf_streams_from(Cursor::new(&bytes)),
        Err(GoproError::Malformed(_))
    ));
}

#[test]
fn missing_file_is_an_io_error() {
    assert!(matches!(
        read_hilights("/definitely/not/here.mp4"),
        Err(GoproError::Io(_))
    ));
}

proptest! {
    /// Arbitrary bytes, and arbitrary corruption of a valid file, return an
    /// error or a result but never panic.
    #[test]
    fn never_panics_on_garbage(bytes in proptest::collection::vec(any::<u8>(), 0..512)) {
        let _ = read_hilights_from(Cursor::new(&bytes));
        let _ = gpmf_streams_from(Cursor::new(&bytes));
    }

    #[test]
    fn never_panics_on_corrupted_fixture(at in 0usize..4096, value in any::<u8>()) {
        let mut bytes = build(&Spec::default());
        let i = at % bytes.len();
        bytes[i] = value;
        let _ = read_hilights_from(Cursor::new(&bytes));
        let _ = gpmf_streams_from(Cursor::new(&bytes));
    }
}
