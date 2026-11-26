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

### Chaptered recordings

A GoPro recording the camera split into chapter files (`GX01xxxx.MP4`,
`GX02xxxx.MP4`, ...) is one `MediaItem` with `chapters`. `ChapteredReader`
is a `VideoReader` over one reader per chapter file: item time `t` maps to
the chapter whose span (from the sum of the earlier chapters' durations)
holds it, frames come out with pts in item time, and `next_frame` runs on
into the next file. A frame at or past its (non-last) chapter's duration is
dropped, so the boundary has no duplicate. Files open on first use; at most
two stay open. `open_item_video(backend, path, chapters, opts)` returns the
plain reader when `chapters` is empty and a `ChapteredReader` otherwise.
Clip audio across chapters is not handled yet.

Errors: `Open` (unreadable or truncated container), `NoVideoStream`,
`Decode` (corrupt data, unsupported codec). `FfmpegBackend::open_video_reader`
returns the concrete reader, which also reports `seek_count()` and
`hardware()`.

### Hardware decode

On macOS, `FfmpegBackend` decodes H.264 and HEVC (8- and 10-bit) with
VideoToolbox through FFmpeg's `videotoolbox` hwaccel when it can; everything
else, and every other platform, decodes on the CPU with FFmpeg's frame
threading. Hardware frames are copied to system memory (NV12, or P010 for
10-bit) and go through the same swscale conversion as CPU frames, so sizes,
rotation, colour matrix and range are handled identically and the frames
are the same RGBA8 `VideoFrame`s.

The reader first asks VideoToolbox (`VTIsHardwareDecodeSupported`) whether
this Mac has a hardware decoder for the codec; a virtualised Mac usually has
none and decodes on the CPU without trying (and without FFmpeg printing
hwaccel errors). If the hardware path fails at any point (no VideoToolbox device, a codec or
profile it cannot decode, an error mid-stream) the reader logs it once at
`debug` and continues on the CPU with no error to the caller: a failure on
the first frame reopens the file on the CPU (the first frame is decoded
when the reader is opened, so `hardware()` is settled from then on), a
later one resumes right after the last frame produced.

- `FfmpegVideoReader::hardware()` says which path is in use; the seek
  benchmark prints it per case.
- `KV_HWDECODE=0` (also `false`, `off`, `no`) forces CPU decode, for
  before/after comparisons and as an escape hatch.
  `FfmpegBackend::open_video_reader_with(path, opts, HwDecode::Cpu)` does the
  same for one reader regardless of the variable.
- A shipped FFmpeg build must be configured with `--enable-videotoolbox`
  (Homebrew's is).

## Filmstrip thumbnails

`filmstrip(reader, duration, count, height)` returns `count` (1..=64)
thumbnails `height` (16..=360) pixels high; thumbnail `i` is the frame
displayed at `duration * (i + 0.5) / count`, visited in increasing order (no
backwards seeks). Width keeps the display aspect ratio after rotation,
rounded to even (`filmstrip_size(&VideoInfo, height)`); open the reader with
`DecodeOptions { size: Some(filmstrip_size(..)?), .. }` so the decoder
scales each frame once. Frames of another size are area-averaged.

Thumbnails are fetched with `VideoReader::frame_near(t, 0.5 s)`: when a cell
needs a seek, the keyframe the seek lands on is used as is if it is at most
0.5 s before the cell centre (`FILMSTRIP_KEYFRAME_SNAP`), instead of
decoding up to a GOP forward. Cells reached by decoding forward (within 2 s)
are exact. The default `frame_near` is `frame_at`. `encode_jpeg(&frame,
quality)` writes a baseline JPEG (the `image` crate, `jpeg` feature only; no
FFmpeg needed) for caching and serving thumbnails.

## Frame and export types

`VideoFrame` is one frame in CPU memory: presentation time (`Micros`), size
and tightly packed `PixelFormat::Rgba8` bytes (sRGB, straight alpha).
`ExportSettings` (size, frame rate, `VideoCodec::H264` or `VideoCodec::Hevc`
(serde `"hevc"`), bitrate, audio rate and channels; serde-serialisable,
`validate()` checks it is encodable: even sizes up to 4096x4096, which covers
4K in both orientations) is the shared vocabulary for decode, encode and the
compositor. `ExportSettings::default_bitrate(w, h, codec)` gives a starting
bitrate (guesses, scaled by pixel count): H.264 12 Mbit/s at 1080p and
45 Mbit/s at 3840x2160, HEVC 60% of that. Both live in
`frame.rs` and need no FFmpeg.

## Export (`Encoder`)

`Encoder::create(path, &ExportSettings)` returns a `VideoWriter` that takes
RGBA `VideoFrame`s (in order, at exactly the export frame rate; each frame's
`pts` is its timestamp) and interleaved `f32` PCM in chunks of any size, and
writes an MP4 with one H.264 or HEVC stream and one AAC stream: BT.709 limited range (tagged
in the stream), `+faststart` (moov first), both streams starting at 0. AAC
is FFmpeg's native `aac` encoder at 192 kbit/s; a writer that never gets
audio produces a video-only MP4. Dropping a writer without `finish()` leaves
an incomplete file.

`Encoder::supports(codec)` says whether a codec can be encoded here (the UI
hides the others); `create` returns `MediaError::Unsupported` for the rest
without touching the file. `VideoWriter::encoder_name()` says which encoder
was picked:

| Codec | macOS | Linux / Windows |
| --- | --- | --- |
| H.264 | `h264_videotoolbox`, else `libopenh264`, else `libx264` (GPL, dev/CI only) | `libopenh264`, else `libx264` (GPL, dev/CI only) |
| HEVC | `hevc_videotoolbox` (hardware), `hvc1` tag so QuickTime plays it; `Unsupported` when the Mac has no hardware HEVC encoder | `Unsupported("HEVC export needs VideoToolbox")` |

HEVC never falls back to `libx265`: it is GPL, and there is no LGPL software
HEVC encoder in FFmpeg. HEVC output is 8-bit Main profile.

H.264 encoders are tried in this order:

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
`libavcodec`, `libavutil`, `libswscale`), **version 6.1 or newer**, found with
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
