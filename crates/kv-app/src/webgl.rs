//! The WebGL preview fallback (ADR 0007): the preview engine's frames go to
//! the webview as raw RGBA over the `kvframe` custom protocol, and
//! `ui/js/viewer-webgl.js` draws them with WebGL2. The backend on Linux and
//! Windows, and on macOS with `KV_PREVIEW=webgl`
//! ([`crate::preview::PreviewBackend::WebGl`]).
//!
//! - [`WebGlSink`] is the engine's [`FrameSink`]: it keeps the latest
//!   composited CPU frame in [`LatestFrame`], scaled down to fit
//!   [`MAX_FRAME_SIZE`] (aspect kept) if needed, and numbers it with a
//!   sequence number that only grows.
//! - `kvframe://latest?after=<seq>` (Windows: `http://kvframe.localhost/
//!   latest?after=<seq>`, see [`frame_response`]) answers 200 with that
//!   frame when its sequence number is greater than `after`, with
//!   `X-Frame-Width`, `X-Frame-Height`, `X-Frame-Seq` and `X-Frame-Pts`
//!   headers, or 204 when nothing newer exists. The UI polls it once per
//!   animation frame.
//!
//! Portable and GPU-free, so the gate tests it on Linux.

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use kv_core::{AspectRatio, Micros};
use kv_media::{PixelFormat, VideoFrame};
use kv_render::preview::{FrameSink, RenderedFrame, PROXY_SIZE};
use tauri::http::{header, Response, StatusCode, Uri};
use tauri::Runtime;

use crate::preview::{render_test_pattern, test_pattern_size, TEST_PATTERN_FPS};

/// The custom protocol serving preview frames.
pub const FRAME_SCHEME: &str = "kvframe";
/// Largest frame sent to the webview: the 960x540 proxy (ADR 0007: raw
/// 540p held 57 fps on the CI VM; full resolution is out of scope).
pub const MAX_FRAME_SIZE: (u32, u32) = PROXY_SIZE;

/// Response headers describing the frame in the body.
pub const WIDTH_HEADER: &str = "X-Frame-Width";
pub const HEIGHT_HEADER: &str = "X-Frame-Height";
pub const SEQ_HEADER: &str = "X-Frame-Seq";
pub const PTS_HEADER: &str = "X-Frame-Pts";

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    // A plain value swap: nothing a panic could leave half-done.
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The newest preview frame and its sequence number (1 for the first
/// frame, then one more per frame; 0 means none yet). Shared by the sink
/// (engines come and go with the project's aspect ratio) and the protocol.
#[derive(Debug, Default)]
pub struct LatestFrame {
    inner: Mutex<(u64, Option<Arc<VideoFrame>>)>,
}

impl LatestFrame {
    /// Keeps `frame` as the newest one, scaled down to fit
    /// [`MAX_FRAME_SIZE`] when larger, and returns its sequence number.
    /// `None` (nothing kept) for a frame that is not tightly packed RGBA.
    pub fn publish(&self, frame: &VideoFrame) -> Option<u64> {
        let PixelFormat::Rgba8 = frame.format;
        if frame.width == 0
            || frame.height == 0
            || frame.data.len() != frame.width as usize * frame.height as usize * 4
        {
            return None;
        }
        let size = capped_size((frame.width, frame.height));
        let kept = if size == (frame.width, frame.height) {
            frame.clone()
        } else {
            downscale(frame, size)
        };
        let mut g = lock(&self.inner);
        g.0 += 1;
        g.1 = Some(Arc::new(kept));
        Some(g.0)
    }

    /// The sequence number of the newest frame (0 before the first).
    pub fn seq(&self) -> u64 {
        lock(&self.inner).0
    }

    /// The newest frame and its sequence number, if newer than `after`.
    pub fn newer_than(&self, after: u64) -> Option<(u64, Arc<VideoFrame>)> {
        let g = lock(&self.inner);
        match &g.1 {
            Some(f) if g.0 > after => Some((g.0, Arc::clone(f))),
            _ => None,
        }
    }
}

/// `size` fitted inside [`MAX_FRAME_SIZE`] (aspect kept), or `size` itself
/// when it fits already. Never upscales.
pub fn capped_size((w, h): (u32, u32)) -> (u32, u32) {
    let (mw, mh) = MAX_FRAME_SIZE;
    if w <= mw && h <= mh {
        (w, h)
    } else {
        kv_render::source_size_for(mw, mh, w, h, kv_render::Fit::Fit)
    }
}

/// Nearest-neighbour scaling of an RGBA frame to `size` (a preview proxy;
/// the CPU compositor normally renders at that size already).
fn downscale(frame: &VideoFrame, (w, h): (u32, u32)) -> VideoFrame {
    let (sw, sh) = (frame.width as usize, frame.height as usize);
    let (w, h) = (w.max(1) as usize, h.max(1) as usize);
    let mut data = Vec::with_capacity(w * h * 4);
    for y in 0..h {
        let sy = (y * sh + sh / 2) / h;
        let row = &frame.data[sy.min(sh - 1) * sw * 4..][..sw * 4];
        for x in 0..w {
            let sx = ((x * sw + sw / 2) / w).min(sw - 1);
            data.extend_from_slice(&row[sx * 4..sx * 4 + 4]);
        }
    }
    VideoFrame {
        pts: frame.pts,
        width: w as u32,
        height: h as u32,
        format: PixelFormat::Rgba8,
        data,
    }
}

/// The preview engine's [`FrameSink`] for the WebGL backend. Clones share
/// one [`LatestFrame`].
#[derive(Debug, Clone, Default)]
pub struct WebGlSink {
    latest: Arc<LatestFrame>,
}

impl WebGlSink {
    pub fn new(latest: Arc<LatestFrame>) -> WebGlSink {
        WebGlSink { latest }
    }
}

impl FrameSink for WebGlSink {
    fn present(&mut self, frame: &RenderedFrame<'_>) {
        #[allow(irrefutable_let_patterns)] // only one variant without `gpu`
        if let RenderedFrame::Cpu(f) = frame {
            self.latest.publish(f);
        } else {
            // The WebGL backend builds CPU compositors only.
            tracing::debug!("webgl preview: GPU frame ignored");
        }
    }
}

/// Tauri managed state of the WebGL backend: the sink engines draw into
/// and whether the test pattern was started.
#[derive(Debug)]
pub struct WebGlPreview {
    sink: WebGlSink,
    pattern: std::sync::Once,
}

impl WebGlPreview {
    pub fn new(latest: Arc<LatestFrame>) -> WebGlPreview {
        WebGlPreview {
            sink: WebGlSink::new(latest),
            pattern: std::sync::Once::new(),
        }
    }

    /// A sink for a new engine (they all share the newest frame).
    pub fn sink(&self) -> WebGlSink {
        self.sink.clone()
    }

    /// Starts the test pattern at `aspect` unless it already runs.
    pub fn start_test_pattern_once(&self, aspect: AspectRatio) {
        self.pattern
            .call_once(|| start_test_pattern(self.sink(), aspect));
    }
}

/// The `after` sequence number a `kvframe` URL asks for:
/// `kvframe://latest?after=3` (macOS, Linux), `kvframe://localhost/latest`
/// or `http://kvframe.localhost/latest?after=3` (Windows). A missing
/// `after` means 0 (any frame).
pub fn parse_frame_uri(uri: &Uri) -> Result<u64, String> {
    let host = uri.host().unwrap_or("");
    let path = uri.path().trim_matches('/');
    let target = if host.is_empty() || host == "localhost" || host.ends_with(".localhost") {
        path
    } else if path.is_empty() {
        host
    } else {
        return Err(format!("unknown frame URL {uri}"));
    };
    if target != "latest" {
        return Err(format!("unknown frame URL {uri}"));
    }
    let mut after = 0;
    for pair in uri.query().unwrap_or("").split('&') {
        if let Some(v) = pair.strip_prefix("after=") {
            after = v
                .parse()
                .map_err(|_| format!("bad frame sequence number {v:?}"))?;
        }
    }
    Ok(after)
}

/// Answers a `kvframe` request from `latest`: 200 with the raw RGBA frame
/// (rows top to bottom, `width * height * 4` bytes) when one newer than
/// `after` exists, 204 when not (with `X-Frame-Seq` set to the newest
/// number), 400 for a malformed URL. Readable cross-origin from the page.
pub fn frame_response(latest: &LatestFrame, uri: &Uri) -> Response<Vec<u8>> {
    let base = || {
        Response::builder()
            .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
            .header(
                header::ACCESS_CONTROL_EXPOSE_HEADERS,
                format!("{WIDTH_HEADER}, {HEIGHT_HEADER}, {SEQ_HEADER}, {PTS_HEADER}"),
            )
            .header(header::CACHE_CONTROL, "no-store")
    };
    let after = match parse_frame_uri(uri) {
        Ok(a) => a,
        Err(e) => {
            return base()
                .status(StatusCode::BAD_REQUEST)
                .header(header::CONTENT_TYPE, "text/plain")
                .body(e.into_bytes())
                .expect("valid error response")
        }
    };
    let response = match latest.newer_than(after) {
        Some((seq, frame)) => base()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "application/octet-stream")
            .header(WIDTH_HEADER, frame.width)
            .header(HEIGHT_HEADER, frame.height)
            .header(SEQ_HEADER, seq)
            .header(PTS_HEADER, frame.pts.0)
            .body(frame.data.clone()),
        None => base()
            .status(StatusCode::NO_CONTENT)
            .header(SEQ_HEADER, latest.seq())
            .body(Vec::new()),
    };
    response.expect("valid frame response")
}

/// Registers the `kvframe` protocol on `builder`, serving `latest`.
pub fn register<R: Runtime>(
    builder: tauri::Builder<R>,
    latest: Arc<LatestFrame>,
) -> tauri::Builder<R> {
    // Answered inline: it only clones the newest frame (~2 MB at 540p).
    builder.register_uri_scheme_protocol(FRAME_SCHEME, move |_ctx, request| {
        frame_response(&latest, request.uri())
    })
}

/// Plays the moving test pattern (`--preview-test-pattern`) at the
/// project's aspect ratio through `sink`, forever, on its own thread.
pub fn start_test_pattern(mut sink: WebGlSink, aspect: AspectRatio) {
    let (w, h) = capped_size(test_pattern_size(aspect));
    let spawned = std::thread::Builder::new()
        .name("kv-preview-test-pattern".into())
        .spawn(move || {
            tracing::info!(width = w, height = h, "webgl preview test pattern running");
            let mut frame = VideoFrame::solid(Micros(0), w, h, [0, 0, 0, 255]);
            let period = Duration::from_micros(1_000_000 / TEST_PATTERN_FPS as u64);
            for n in 0.. {
                render_test_pattern(n, &mut frame);
                sink.present(&RenderedFrame::Cpu(&frame));
                std::thread::sleep(period);
            }
        });
    if let Err(e) = spawned {
        tracing::warn!("cannot start the preview test pattern: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn uri(s: &str) -> Uri {
        s.parse().unwrap()
    }

    fn header_of(r: &Response<Vec<u8>>, name: &str) -> String {
        r.headers()[name].to_str().unwrap().to_string()
    }

    #[test]
    fn parses_frame_urls_on_every_platform() {
        assert_eq!(parse_frame_uri(&uri("kvframe://latest?after=7")), Ok(7));
        assert_eq!(parse_frame_uri(&uri("kvframe://latest")), Ok(0));
        assert_eq!(
            parse_frame_uri(&uri("kvframe://localhost/latest?after=3")),
            Ok(3)
        );
        assert_eq!(
            parse_frame_uri(&uri("http://kvframe.localhost/latest?x=1&after=12")),
            Ok(12)
        );
        for bad in [
            "kvframe://latest?after=-1",
            "kvframe://latest?after=x",
            "kvframe://other",
            "kvframe://latest/1",
            "http://kvframe.localhost/oldest",
        ] {
            assert!(parse_frame_uri(&uri(bad)).is_err(), "{bad}");
        }
    }

    #[test]
    fn no_content_before_the_first_frame() {
        let latest = LatestFrame::default();
        let r = frame_response(&latest, &uri("kvframe://latest?after=0"));
        assert_eq!(r.status(), StatusCode::NO_CONTENT);
        assert!(r.body().is_empty());
        assert_eq!(header_of(&r, SEQ_HEADER), "0");
        assert_eq!(header_of(&r, "access-control-allow-origin"), "*");
    }

    #[test]
    fn serves_the_frame_with_headers_then_204_until_a_newer_one() {
        let latest = Arc::new(LatestFrame::default());
        let mut sink = WebGlSink::new(Arc::clone(&latest));
        let red = VideoFrame::solid(Micros(40_000), 16, 9, [255, 0, 0, 255]);
        sink.present(&RenderedFrame::Cpu(&red));

        let r = frame_response(&latest, &uri("kvframe://latest?after=0"));
        assert_eq!(r.status(), StatusCode::OK);
        assert_eq!(header_of(&r, "content-type"), "application/octet-stream");
        assert_eq!(header_of(&r, WIDTH_HEADER), "16");
        assert_eq!(header_of(&r, HEIGHT_HEADER), "9");
        assert_eq!(header_of(&r, SEQ_HEADER), "1");
        assert_eq!(header_of(&r, PTS_HEADER), "40000");
        assert_eq!(header_of(&r, "cache-control"), "no-store");
        let exposed = header_of(&r, "access-control-expose-headers");
        for h in [WIDTH_HEADER, HEIGHT_HEADER, SEQ_HEADER, PTS_HEADER] {
            assert!(exposed.contains(h), "{h} exposed to the page");
        }
        assert_eq!(r.body(), &red.data);

        // Already seen: nothing newer.
        let r = frame_response(&latest, &uri("kvframe://latest?after=1"));
        assert_eq!(r.status(), StatusCode::NO_CONTENT);
        assert_eq!(header_of(&r, SEQ_HEADER), "1");

        // A newer frame gets the next number; asking after an older one
        // returns only the newest.
        let green = VideoFrame::solid(Micros(80_000), 16, 9, [0, 255, 0, 255]);
        sink.present(&RenderedFrame::Cpu(&green));
        sink.present(&RenderedFrame::Cpu(&red));
        let r = frame_response(&latest, &uri("http://kvframe.localhost/latest?after=1"));
        assert_eq!(r.status(), StatusCode::OK);
        assert_eq!(header_of(&r, SEQ_HEADER), "3");
        assert_eq!(r.body(), &red.data);
        let r = frame_response(&latest, &uri("kvframe://latest?after=3"));
        assert_eq!(r.status(), StatusCode::NO_CONTENT);
        // A client ahead of the backend (e.g. stale state) gets 204 too.
        let r = frame_response(&latest, &uri("kvframe://latest?after=99"));
        assert_eq!(r.status(), StatusCode::NO_CONTENT);
    }

    #[test]
    fn bad_urls_are_400() {
        let latest = LatestFrame::default();
        let r = frame_response(&latest, &uri("kvframe://latest?after=abc"));
        assert_eq!(r.status(), StatusCode::BAD_REQUEST);
        assert_eq!(header_of(&r, "access-control-allow-origin"), "*");
    }

    #[test]
    fn frames_are_capped_at_proxy_size_keeping_aspect() {
        let latest = LatestFrame::default();
        let cases = [
            ((1920, 1080), (960, 540)),
            ((1080, 1920), (304, 540)),
            ((1080, 1080), (540, 540)),
            ((640, 360), (640, 360)),
        ];
        for ((w, h), want) in cases {
            let mut f = VideoFrame::solid(Micros(0), w, h, [0, 0, 255, 255]);
            // A white left half survives scaling on the left.
            for row in f.data.chunks_exact_mut(w as usize * 4) {
                for px in row[..(w as usize / 2) * 4].chunks_exact_mut(4) {
                    px.copy_from_slice(&[255, 255, 255, 255]);
                }
            }
            latest.publish(&f).unwrap();
            let (_, kept) = latest.newer_than(0).unwrap();
            assert_eq!((kept.width, kept.height), want, "{w}x{h}");
            assert!(kept.width <= MAX_FRAME_SIZE.0 && kept.height <= MAX_FRAME_SIZE.1);
            assert_eq!(kept.data.len(), (want.0 * want.1 * 4) as usize);
            assert_eq!(&kept.data[..4], &[255, 255, 255, 255]);
            assert_eq!(&kept.data[kept.data.len() - 4..], &[0, 0, 255, 255]);
        }
    }

    #[test]
    fn malformed_frames_are_not_kept() {
        let latest = LatestFrame::default();
        let mut f = VideoFrame::solid(Micros(0), 4, 4, [0, 0, 0, 255]);
        f.data.truncate(8);
        assert_eq!(latest.publish(&f), None);
        assert_eq!(latest.seq(), 0);
    }
}
