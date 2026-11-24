# 0006 GoPro metadata: pure-Rust reader for HiLights and GPMF

Status: accepted (2026-11-24)

## Context

Quik's auto-edit centres on HiLight tags: moments the user marked on the
camera (button press, voice command or, on some models, a shake/tap). GoPro
cameras from HERO5 onwards also record telemetry (accelerometer, gyro, GPS,
...) in GPMF, the GoPro Metadata Format. kwikvid needs both in M1, behind the
`kv-gopro` crate.

### Where the data lives

GoPro MP4 files hold:

- **GPMF telemetry track** (HERO5 onwards; documented in gopro/gpmf-parser):
  a `trak` with handler `meta`, name "GoPro MET", whose sample description is
  `gpmd`. Each sample is about one second of GPMF: big-endian, 32-bit aligned
  KLV entries, `FourCC | type (1) | struct size (1) | repeat (u16) | data`.
  Type 0 nests. A payload is `DEVC` (device) entries holding `STRM` (stream)
  entries; inside a `STRM` the sticky metadata (`STNM`, `SIUN`, `SCAL`, ...)
  comes first and the last entry is the sensor data (`ACCL`, `GYRO`, `GPS5`,
  ...), whose `repeat` is its sample count.
- **`moov/udta/HMMT`**: the HiLight box. Layout as read by VLC's MP4 demuxer
  (`MP4_ReadBox_HMMT`, 2015): big-endian `u32` count, then `count` big-endian
  `u32` offsets in milliseconds from the start of the file. Confirmed for
  HERO4/HERO5-era files by VLC and community tools.
- **`moov/udta/GPMF`**: a non-timed GPMF blob of global settings (firmware,
  serial, capture settings). Community parsers report that from about HERO6
  onwards HiLights are (also?) stored here, as an `HLMT` container whose
  `MANL` entries are millisecond offsets (guess: secondhand, not in GoPro's
  published docs, not confirmed against a file).
- HiLights added later in the GoPro Quik app are not written back to the MP4
  (stated by two independent community parsers); they live in Quik's own
  database and are out of reach.
- GoPro's gpmf-parser README documents the telemetry track in detail but does
  not mention HiLights at all.

## Options considered

1. **bindgen over gpmf-parser (C).** Official, Apache-2.0 OR MIT. But it only
   parses GPMF; the MP4 demux in its demo (`GPMF_mp4reader.c`) is example code,
   it does not read `HMMT`, and it brings a C toolchain, bindgen/libclang in
   CI and `unsafe` FFI for what is about 300 lines of parsing.
2. **Pure-Rust parser (chosen).** A minimal ISO-BMFF box walker (seek past
   `mdat`, load only `moov` and the GPMF samples) plus a GPMF KLV iterator.
   No `unsafe`, no build-time tooling, portable, fuzzable with proptest.
3. **Existing crates.** `gpmf` (crates.io, MIT/Apache) parses KLV but not the
   MP4 container or HiLights; `telemetry-parser` (Gyroflow) is broad and
   capable but GPL-3.0, so it is excluded by ADR 0005; `mp4` / `mp4parse`
   (MIT / MPL-2.0) would demux but do not expose `udta/HMMT` or the `gpmd`
   sample entry without patching, and pull in more than the few boxes needed.

## Licence check

gpmf-parser: Apache-2.0 OR MIT (we read its docs, we do not vendor code).
The chosen implementation adds no new dependencies: only `kv-core` and
`thiserror` (already in the workspace); `proptest` for tests.

## Decision

`crates/kv-gopro` is a pure-Rust reader:

- `read_hilights(path) -> Result<Vec<Micros>>` reads `moov/udta/HMMT`,
  converts milliseconds to `Micros`, returns them sorted; empty when absent.
- `gpmf_streams(path) -> Result<Vec<StreamInfo>>` finds the `gpmd` track,
  walks its sample table (`stsz`, `stsc`, `stco`/`co64`), parses every payload
  and returns one entry per sensor FourCC: optional `STNM` name, total sample
  count, and average rate (samples / `mdhd` duration).
- Both have `_from(Read + Seek)` variants for tests and future callers.
- Corrupt input returns `GoproError::Malformed`, never panics (proptest); box
  and sample sizes are capped before allocating.

`HLMT`/`MANL` in `udta/GPMF` is deliberately **not** read yet: the issue
asks not to guess in code, and its layout is only secondhand. Supporting it
is tracked as a follow-up together with the other questions below.

### Test fixtures

No openly licensed GoPro sample with HiLights exists, so
`crates/kv-gopro/tests/common/mod.rs` writes synthetic MP4s by hand: `ftyp`,
`mdat` with GPMF payloads, and `moov` holding `udta/HMMT`, a stub video track
(`avc1`) that must be skipped, and a `gpmd` track whose sample table points
into `mdat` (also a fast-start `moov`-first variant with `co64`). Real
footage goes into the regression suite later (ADR 0004).

## Open questions (need real footage)

1. Which camera generations write `HMMT`, which write `udta/GPMF` `HLMT`/`MANL`,
   and do any write both? Exact `HLMT`/`MANL` layout (type, units, nesting).
2. Are HiLight offsets relative to the start of each chapter file (GX01..,
   GX02..) or to the start of the recording? Matters for chaptered grouping.
3. Are HiLights from voice command, button and auto-detection distinguishable?
4. Real `moov` and GPMF payload sizes for long 4K recordings (current caps:
   256 MiB `moov`, 16 MiB per sample).
5. Does `STRM` ever hold more than one data key, or data larger than 64 KiB
   per entry (gpmf-parser has an extended-size path we do not handle)?
6. Do `.LRV` proxies carry the same HiLights and telemetry as the MP4?
7. Chapter grouping (`kv_gopro::group_chapters`) is purely filename-based
   (`GOPRxxxx`/`GPzzxxxx`, `GHzzxxxx`, `GXzzxxxx`), taken from GoPro's public
   naming article and unverified on real cards (guess). Confirm the scheme per
   camera generation, and whether `udta` metadata or creation times are needed
   to tell recordings apart once numbers wrap.

## Consequences

- No C toolchain or bindgen; `kv-gopro` builds everywhere the core does.
- We own a small parser and must keep it in step with new cameras; the
  regression suite on real footage is the safety net.
- `docs/architecture.md` lists kv-gopro without the gpmf-parser dependency.
