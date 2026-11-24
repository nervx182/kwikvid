# kv-media

kwikvid's platform media layer. Everything that reads or writes media files
goes through the `MediaBackend` trait, so the engine crates stay free of I/O
(see `docs/architecture.md`). Today the trait has one method:

```rust
fn probe(&self, path: &Path) -> Result<MediaInfo>;
```

`MediaInfo` holds the duration (`kv_core::Micros`), container name, the
primary video stream (codec, coded size, frame rate as a rational, bit depth,
clockwise display rotation), the primary audio stream (codec, sample rate,
channels) and the creation time (microseconds since the Unix epoch, UTC).

## Frame and export types

`VideoFrame` is one frame in CPU memory: presentation time (`Micros`), size
and tightly packed `PixelFormat::Rgba8` bytes (sRGB, straight alpha).
`ExportSettings` (size, frame rate, `VideoCodec::H264`, bitrate, audio rate
and channels; serde-serialisable, `validate()` checks it is encodable) is the
shared vocabulary for decode, encode and the compositor. Both live in
`frame.rs` and need no FFmpeg.

## Export (`Encoder`)

`Encoder::create(path, &ExportSettings)` returns a `VideoWriter` that takes
RGBA `VideoFrame`s (in order, at exactly the export frame rate; each frame's
`pts` is its timestamp) and interleaved `f32` PCM in chunks of any size, and
writes an MP4 with one H.264 and one AAC stream: BT.709 limited range (tagged
in the stream), `+faststart` (moov first), both streams starting at 0. AAC
is FFmpeg's native `aac` encoder at 192 kbit/s; a writer that never gets
audio produces a video-only MP4. Dropping a writer without `finish()` leaves
an incomplete file.

H.264 encoders are tried in this order; `VideoWriter::encoder_name()` says
which one was picked:

1. `h264_videotoolbox` (macOS only, hardware). If it cannot be opened (for
   example a virtualised runner without a hardware encoder) the writer falls
   back with a `tracing` warning. Set `KV_REQUIRE_VIDEOTOOLBOX=1` to make
   that an error instead (manual checks on a real Mac; on other systems it
   always fails).
2. `libopenh264` (BSD; Cisco's binary is patent-licensed).
3. `libx264`: **GPL**, logged with a warning. Development and CI only; it
   must never be the encoder of a shipped build (see Licensing).

## Backends

| Backend | Cargo feature | Notes |
| --- | --- | --- |
| `FfmpegBackend` | `ffmpeg` (default) | System FFmpeg through `ffmpeg-next` |

Build without FFmpeg (types and trait only) with
`cargo build -p kv-media --no-default-features`.

## System FFmpeg requirement

The `ffmpeg` feature links the system FFmpeg libraries (`libavformat`,
`libavcodec`, `libavutil`), **version 6.1 or newer**, found with
`pkg-config`. Building the bindings also needs `clang`/libclang (bindgen).

- Ubuntu/Debian:
  `sudo apt-get install libavcodec-dev libavformat-dev libavutil-dev libswscale-dev pkg-config clang libclang-dev`
- macOS: `brew install ffmpeg pkgconf`
- Windows (MSVC): unpack a prebuilt shared FFmpeg (the regression workflow
  uses the newest BtbN `ffmpeg-n*-latest-win64-gpl-shared-*.zip`), set `FFMPEG_DIR`
  to it, put its `bin` on `PATH` and set `LIBCLANG_PATH` to LLVM's `bin`
  (e.g. `C:\Program Files\LLVM\bin`).

Export also needs `libswscale` (`libswscale-dev` on Ubuntu/Debian; part of
the Homebrew formula).

The tests encode their own 2 s fixture clips with the FFmpeg API, so they need
an FFmpeg with the `libx264` (or `libopenh264`), `libx265` (10-bit capable)
and native `aac` encoders. The distribution packages above have them.

## Licensing

Per ADR 0005 (`docs/decisions/0005-dependency-licenses.md`) a shipped kwikvid
must link an **LGPL, dynamically linked FFmpeg built without
`--enable-gpl`**. The Ubuntu, Homebrew and BtbN `gpl` packages are GPL builds; they are
acceptable for development and CI only. In a shipped LGPL build the export
path uses VideoToolbox on macOS and `libopenh264` elsewhere; `libx264` is
not available there, by design. The Rust bindings (`ffmpeg-next`,
`ffmpeg-sys-next`) are WTFPL, allowed by a scoped exception in `deny.toml`.
