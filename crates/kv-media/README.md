# kv-media

kwikvid's platform media layer. Everything that reads or writes media files
goes through the `MediaBackend` trait, so the engine crates stay free of I/O
(see `docs/architecture.md`):

```rust
fn probe(&self, path: &Path) -> Result<MediaInfo>;
fn open_video(&self, path: &Path, opts: &DecodeOptions) -> Result<Box<dyn VideoReader>>;
```

`MediaInfo` holds the duration (`kv_core::Micros`), container name, the
primary video stream (codec, coded size, frame rate as a rational, bit depth,
clockwise display rotation), the primary audio stream (codec, sample rate,
channels) and the creation time (microseconds since the Unix epoch, UTC).

## Decode

`open_video` opens the primary video stream and returns a `VideoReader`
that yields RGBA8 `VideoFrame`s in presentation order:

- `next_frame()`: the next frame, `None` after the last.
- `frame_at(t)`: the frame displayed at `t` (largest pts `<= t`; the last
  frame past the end, `None` before the first). When `t` is ahead and within
  2 s it decodes forward; otherwise it seeks to the keyframe before `t` and
  decodes forward. Calls with increasing `t` (an export walking its output
  frames) never re-seek.
- `seek(t)`: the next `next_frame()` returns the frame displayed at `t`.
- `info()`: the stream's `VideoInfo` (as `probe` reports it).

Frame pts are `Micros` relative to the stream start (the first frame of a
normal file is at 0). `DecodeOptions { size, apply_rotation }` controls the
output: `size: None` keeps the display size, `Some((w, h))` scales to exactly
`w x h` with no aspect handling (cropping and fitting are the compositor's
job). `apply_rotation` (default on) applies the display rotation (90/180/270)
before scaling, so `size` is the final upright size. Any input pixel format,
including 10-bit HEVC, is converted to 8-bit RGBA by swscale using the
stream's colour matrix and range (untagged streams: BT.709 from 720 lines up,
else BT.601).

`FfmpegBackend` decodes on the CPU with FFmpeg's frame threading; hardware
decode (VideoToolbox) is a later item. Errors: `Open` (unreadable or
truncated container), `NoVideoStream`, `Decode` (corrupt data, unsupported
codec). `FfmpegBackend::open_video_reader` returns the concrete reader, which
also reports `seek_count()`.

## Frame and export types

`VideoFrame` is one frame in CPU memory: presentation time (`Micros`), size
and tightly packed `PixelFormat::Rgba8` bytes (sRGB, straight alpha).
`ExportSettings` (size, frame rate, `VideoCodec::H264`, bitrate, audio rate
and channels; serde-serialisable, `validate()` checks it is encodable) is the
shared vocabulary for decode, encode and the compositor. Both live in
`frame.rs` and need no FFmpeg.

## Backends

| Backend | Cargo feature | Notes |
| --- | --- | --- |
| `FfmpegBackend` | `ffmpeg` (default) | System FFmpeg through `ffmpeg-next` |

Build without FFmpeg (types and trait only) with
`cargo build -p kv-media --no-default-features`.

## System FFmpeg requirement

The `ffmpeg` feature links the system FFmpeg libraries (`libavformat`,
`libavcodec`, `libavutil`, `libswscale`), **version 6.1 or newer**, found with
`pkg-config`. Building the bindings also needs `clang`/libclang (bindgen).

- Ubuntu/Debian:
  `sudo apt-get install libavcodec-dev libavformat-dev libavutil-dev libswscale-dev pkg-config clang libclang-dev`
- macOS: `brew install ffmpeg pkgconf`
- Windows (MSVC): unpack a prebuilt shared FFmpeg (the regression workflow
  uses the newest BtbN `ffmpeg-n*-latest-win64-gpl-shared-*.zip`), set `FFMPEG_DIR`
  to it, put its `bin` on `PATH` and set `LIBCLANG_PATH` to LLVM's `bin`
  (e.g. `C:\Program Files\LLVM\bin`).

The tests encode their own 2 s fixture clips with the FFmpeg API, so they need
an FFmpeg with the `libx264` (or `libopenh264`), `libx265` (10-bit capable)
and native `aac` encoders. The distribution packages above have them.

## Licensing

Per ADR 0005 (`docs/decisions/0005-dependency-licenses.md`) a shipped kwikvid
must link an **LGPL, dynamically linked FFmpeg built without
`--enable-gpl`**. The Ubuntu, Homebrew and BtbN `gpl` packages are GPL builds; they are
acceptable for development and CI only. The Rust bindings (`ffmpeg-next`,
`ffmpeg-sys-next`) are WTFPL, allowed by a scoped exception in `deny.toml`.
