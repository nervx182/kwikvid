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
  `sudo apt-get install libavcodec-dev libavformat-dev libavutil-dev pkg-config clang libclang-dev`
- macOS: `brew install ffmpeg pkgconf`

The tests encode their own 2 s fixture clips with the FFmpeg API, so they need
an FFmpeg with the `libx264` (or `libopenh264`), `libx265` (10-bit capable)
and native `aac` encoders. The distribution packages above have them.

## Licensing

Per ADR 0005 (`docs/decisions/0005-dependency-licenses.md`) a shipped kwikvid
must link an **LGPL, dynamically linked FFmpeg built without
`--enable-gpl`**. The Ubuntu and Homebrew packages are GPL builds; they are
acceptable for development and CI only. The Rust bindings (`ffmpeg-next`,
`ffmpeg-sys-next`) are WTFPL, allowed by a scoped exception in `deny.toml`.
