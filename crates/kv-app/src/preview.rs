//! Preview backend selection and viewer geometry (ADR 0007). Portable: no
//! AppKit, no GPU, so the gate unit-tests it on Linux.
//!
//! The UI reserves a transparent viewer rectangle and reports it with
//! `set_viewer_rect { x, y, w, h, scale }` (CSS pixels relative to the
//! webview, and `devicePixelRatio`). The backend chosen by
//! [`PreviewBackend::preferred`] then draws the preview there, letterboxed
//! to the frame's (the project's) aspect ratio.

// The geometry and test pattern serve the macOS native layer; elsewhere
// only the tests use them until the WebGL fallback (#76) lands.
#![cfg_attr(not(target_os = "macos"), allow(dead_code))]

use std::sync::{Mutex, PoisonError};

use kv_core::{AspectRatio, Micros};
use kv_media::VideoFrame;

/// How preview frames reach the screen. The one place the backend is
/// chosen; the WebGL fallback (#76) adds its variant here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreviewBackend {
    /// wgpu renders into a `CAMetalLayer` on an `NSView` under the
    /// transparent webview (`native_view`).
    #[cfg(target_os = "macos")]
    NativeMetal,
    /// No preview surface: the viewer stays empty. Used where no backend
    /// exists yet and when the preferred one fails to start.
    Off,
}

impl PreviewBackend {
    /// The backend this build uses unless it fails to start.
    pub fn preferred() -> PreviewBackend {
        #[cfg(target_os = "macos")]
        {
            PreviewBackend::NativeMetal
        }
        #[cfg(not(target_os = "macos"))]
        {
            PreviewBackend::Off
        }
    }

    /// Short name for logs.
    pub fn name(self) -> &'static str {
        match self {
            #[cfg(target_os = "macos")]
            PreviewBackend::NativeMetal => "native-metal",
            PreviewBackend::Off => "off",
        }
    }
}

/// The viewer rectangle as the UI reports it: CSS pixels relative to the
/// webview's top-left corner, plus the device pixel ratio.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ViewerRect {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
    pub scale: f64,
}

impl ViewerRect {
    /// Checks the values are usable: finite, non-negative size, scale in
    /// (0, 8]. Returns a sentence for the UI otherwise.
    pub fn validate(self) -> Result<ViewerRect, String> {
        let ViewerRect { x, y, w, h, scale } = self;
        if ![x, y, w, h, scale].iter().all(|v| v.is_finite()) {
            return Err("The viewer size is not a number.".into());
        }
        if w < 0.0 || h < 0.0 || w > 65_536.0 || h > 65_536.0 {
            return Err(format!("The viewer size {w}x{h} is out of range."));
        }
        if scale <= 0.0 || scale > 8.0 {
            return Err(format!("The display scale {scale} is out of range."));
        }
        Ok(self)
    }

    /// Size in physical (device) pixels, which the drawing surface uses.
    pub fn physical_size(&self) -> (u32, u32) {
        // Values are validated, so the casts cannot overflow.
        let px = |v: f64| (v * self.scale).round().max(0.0) as u32;
        (px(self.w), px(self.h))
    }

    /// True when nothing would be visible.
    pub fn is_empty(&self) -> bool {
        let (w, h) = self.physical_size();
        w == 0 || h == 0
    }
}

/// A rectangle in whole pixels, origin top-left.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PixelRect {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

/// The largest rectangle with `content`'s aspect ratio that fits in
/// `container`, centred (letterbox or pillarbox bars on the sides). Empty
/// when either size is empty.
pub fn letterbox(container: (u32, u32), content: (u32, u32)) -> PixelRect {
    let (cw, ch) = (u64::from(container.0), u64::from(container.1));
    let (fw, fh) = (u64::from(content.0), u64::from(content.1));
    if cw == 0 || ch == 0 || fw == 0 || fh == 0 {
        return PixelRect::default();
    }
    // Rounded integer division.
    let div = |n: u64, d: u64| (n + d / 2) / d;
    let (w, h) = if cw * fh <= ch * fw {
        // Relatively narrower container: full width, bars top and bottom.
        (cw, div(cw * fh, fw).clamp(1, ch))
    } else {
        (div(ch * fw, fh).clamp(1, cw), ch)
    };
    // Both fit in u32: they are at most the container's size.
    PixelRect {
        x: ((cw - w) / 2) as u32,
        y: ((ch - h) / 2) as u32,
        w: w as u32,
        h: h as u32,
    }
}

/// An AppKit view frame (points, origin at the parent's bottom-left unless
/// the parent is flipped).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ViewFrame {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

/// The frame of a child view covering `rect` in a parent `parent_height`
/// points tall. CSS pixels are points at page zoom 1, and the webview
/// fills the parent, so only the y axis may need flipping.
pub fn appkit_frame(rect: &ViewerRect, parent_height: f64, flipped: bool) -> ViewFrame {
    let y = if flipped {
        rect.y
    } else {
        parent_height - rect.y - rect.h
    };
    ViewFrame {
        x: rect.x,
        y,
        w: rect.w,
        h: rect.h,
    }
}

/// Frame size of the moving test pattern for a project aspect ratio.
pub fn test_pattern_size(aspect: AspectRatio) -> (u32, u32) {
    match aspect {
        AspectRatio::Landscape16x9 => (1920, 1080),
        AspectRatio::Portrait9x16 => (1080, 1920),
        AspectRatio::Square1x1 => (1080, 1080),
    }
}

/// Frames of the test pattern per second (the display rate it is paced
/// by is usually 60 Hz).
pub const TEST_PATTERN_FPS: i64 = 60;

/// Draws frame `n` of the moving test pattern into `frame` (RGBA8): eight
/// colour bars scrolling right, a white box bouncing across, and a row of
/// 16 squares showing `n` in binary along the bottom (white = 1). Motion
/// makes dropped or torn frames visible during a manual check.
pub fn render_test_pattern(n: u64, frame: &mut VideoFrame) {
    const BARS: [[u8; 3]; 8] = [
        [235, 235, 235],
        [235, 235, 16],
        [16, 235, 235],
        [16, 235, 16],
        [235, 16, 235],
        [235, 16, 16],
        [16, 16, 235],
        [16, 16, 16],
    ];
    let (w, h) = (frame.width as usize, frame.height as usize);
    frame.pts = Micros(n as i64 * 1_000_000 / TEST_PATTERN_FPS);
    if w == 0 || h == 0 {
        return;
    }
    let shift = (n as usize * 4) % w;
    let bar_w = w.div_ceil(BARS.len());
    let box_side = (h / 6).max(1);
    let travel = w.saturating_sub(box_side).max(1);
    let phase = (n as usize * 8) % (2 * travel);
    let box_x = if phase < travel {
        phase
    } else {
        2 * travel - phase
    };
    let box_y = (h - box_side) / 2;
    let bit_side = (w / 32).max(1);
    let bits_y = h.saturating_sub(bit_side * 2);
    for y in 0..h {
        let row = &mut frame.data[y * w * 4..(y + 1) * w * 4];
        for (x, px) in row.chunks_exact_mut(4).enumerate() {
            let mut rgb = BARS[((x + w - shift) % w) / bar_w];
            if (box_x..box_x + box_side).contains(&x) && (box_y..box_y + box_side).contains(&y) {
                rgb = [255, 255, 255];
            }
            if (bits_y..bits_y + bit_side).contains(&y) {
                let bit = x / bit_side;
                if bit < 16 && x % bit_side != 0 {
                    let on = (n >> (15 - bit)) & 1 == 1;
                    rgb = if on { [255, 255, 255] } else { [0, 0, 0] };
                }
            }
            px.copy_from_slice(&[rgb[0], rgb[1], rgb[2], 255]);
        }
    }
}

/// Tauri managed state of the preview: the active backend.
#[derive(Debug)]
pub struct PreviewState {
    backend: Mutex<PreviewBackend>,
    test_pattern: bool,
}

impl PreviewState {
    pub fn new(backend: PreviewBackend, test_pattern: bool) -> PreviewState {
        PreviewState {
            backend: Mutex::new(backend),
            test_pattern,
        }
    }

    /// The backend in use.
    pub fn backend(&self) -> PreviewBackend {
        *self.backend.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Switches to `backend` (to [`PreviewBackend::Off`] when the preferred
    /// one fails to start). Only the native backend can fail so far.
    pub fn set_backend(&self, backend: PreviewBackend) {
        *self.backend.lock().unwrap_or_else(PoisonError::into_inner) = backend;
    }

    /// Whether `--preview-test-pattern` was given.
    pub fn test_pattern(&self) -> bool {
        self.test_pattern
    }
}

/// Picks the surface format to present in: a non-sRGB one, because preview
/// frames hold gamma-encoded values that must be written as they are
/// (`GpuCompositor::render_to_view`). BGRA first (what Metal prefers).
pub fn pick_surface_format(formats: &[wgpu::TextureFormat]) -> Option<wgpu::TextureFormat> {
    use wgpu::TextureFormat as F;
    [F::Bgra8Unorm, F::Rgba8Unorm]
        .into_iter()
        .find(|f| formats.contains(f))
        .or_else(|| formats.iter().copied().find(|f| !f.is_srgb()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: f64, y: f64, w: f64, h: f64, scale: f64) -> ViewerRect {
        ViewerRect { x, y, w, h, scale }
    }

    #[test]
    fn letterbox_fits_each_project_aspect() {
        // 16:9 in a square: bars top and bottom.
        assert_eq!(
            letterbox((1000, 1000), (1920, 1080)),
            PixelRect {
                x: 0,
                y: 218,
                w: 1000,
                h: 563
            }
        );
        // 9:16 in a 16:9 viewer: pillarbox.
        assert_eq!(
            letterbox((1600, 900), (1080, 1920)),
            PixelRect {
                x: 547,
                y: 0,
                w: 506,
                h: 900
            }
        );
        // 1:1 in a wide viewer.
        assert_eq!(
            letterbox((1200, 800), (1080, 1080)),
            PixelRect {
                x: 200,
                y: 0,
                w: 800,
                h: 800
            }
        );
        // Same aspect: fills exactly.
        assert_eq!(
            letterbox((1280, 720), (1920, 1080)),
            PixelRect {
                x: 0,
                y: 0,
                w: 1280,
                h: 720
            }
        );
    }

    #[test]
    fn letterbox_of_empty_sizes_is_empty() {
        assert_eq!(letterbox((0, 100), (16, 9)), PixelRect::default());
        assert_eq!(letterbox((100, 100), (0, 9)), PixelRect::default());
    }

    #[test]
    fn letterbox_stays_inside_and_keeps_aspect() {
        for (cw, ch) in [(1, 1), (3, 1000), (1000, 3), (1919, 1081), (640, 480)] {
            for aspect in [
                AspectRatio::Landscape16x9,
                AspectRatio::Portrait9x16,
                AspectRatio::Square1x1,
            ] {
                let (fw, fh) = test_pattern_size(aspect);
                let r = letterbox((cw, ch), (fw, fh));
                assert!(r.x + r.w <= cw && r.y + r.h <= ch, "{r:?} in {cw}x{ch}");
                assert!(r.w == cw || r.h == ch, "touches two sides: {r:?}");
                assert!(r.w >= 1 && r.h >= 1);
                // Within one pixel of the exact aspect.
                let err = (f64::from(r.w) * f64::from(fh) - f64::from(r.h) * f64::from(fw)).abs();
                assert!(err <= f64::from(fw.max(fh)), "{r:?} for {fw}x{fh}");
                // Centred: the bars differ by at most one pixel.
                assert!((cw - r.w - r.x).abs_diff(r.x) <= 1);
                assert!((ch - r.h - r.y).abs_diff(r.y) <= 1);
            }
        }
    }

    #[test]
    fn physical_size_applies_scale() {
        assert_eq!(
            rect(0.0, 0.0, 640.5, 360.0, 2.0).physical_size(),
            (1281, 720)
        );
        assert_eq!(rect(0.0, 0.0, 100.0, 50.0, 1.0).physical_size(), (100, 50));
        assert!(rect(10.0, 10.0, 0.2, 100.0, 1.0).is_empty());
    }

    #[test]
    fn validate_rejects_nonsense() {
        assert!(rect(0.0, 0.0, 1.0, 1.0, 2.0).validate().is_ok());
        for bad in [
            rect(f64::NAN, 0.0, 1.0, 1.0, 1.0),
            rect(0.0, 0.0, -1.0, 1.0, 1.0),
            rect(0.0, 0.0, 1.0, f64::INFINITY, 1.0),
            rect(0.0, 0.0, 1.0, 1.0, 0.0),
            rect(0.0, 0.0, 1.0, 1.0, 9.0),
        ] {
            assert!(bad.validate().is_err(), "{bad:?}");
        }
    }

    #[test]
    fn appkit_frame_flips_y_for_unflipped_parent() {
        let r = rect(200.0, 40.0, 640.0, 360.0, 2.0);
        assert_eq!(
            appkit_frame(&r, 800.0, false),
            ViewFrame {
                x: 200.0,
                y: 400.0,
                w: 640.0,
                h: 360.0
            }
        );
        assert_eq!(appkit_frame(&r, 800.0, true).y, 40.0);
    }

    #[test]
    fn state_tracks_backend_and_flag() {
        let s = PreviewState::new(PreviewBackend::preferred(), true);
        assert_eq!(s.backend(), PreviewBackend::preferred());
        s.set_backend(PreviewBackend::Off);
        assert_eq!(s.backend(), PreviewBackend::Off);
        assert!(s.test_pattern());
    }

    #[test]
    fn backend_choice_follows_platform() {
        let b = PreviewBackend::preferred();
        #[cfg(target_os = "macos")]
        assert_eq!(b, PreviewBackend::NativeMetal);
        #[cfg(not(target_os = "macos"))]
        assert_eq!(b, PreviewBackend::Off);
        assert!(!b.name().is_empty());
    }

    #[test]
    fn test_pattern_moves_and_encodes_frame_number() {
        let (w, h) = (320, 180);
        let mut a = VideoFrame::solid(Micros(0), w, h, [0, 0, 0, 0]);
        let mut b = a.clone();
        render_test_pattern(0, &mut a);
        render_test_pattern(1, &mut b);
        assert_ne!(a.data, b.data, "consecutive frames differ");
        assert_eq!(b.pts, Micros(1_000_000 / TEST_PATTERN_FPS));
        assert!(a.data.chunks_exact(4).all(|p| p[3] == 255), "opaque");
        // Frame 1: only the last of the 16 bit squares is white.
        let bit_side = (w / 32) as usize;
        let y = h as usize - bit_side * 2 + bit_side / 2;
        let at = |f: &VideoFrame, bit: usize| {
            let x = bit * bit_side + bit_side / 2;
            f.data[(y * w as usize + x) * 4]
        };
        assert_eq!(at(&b, 15), 255);
        assert_eq!(at(&b, 14), 0);
        assert_eq!(at(&a, 15), 0);
    }

    #[test]
    fn test_pattern_size_matches_aspect() {
        assert_eq!(test_pattern_size(AspectRatio::Landscape16x9), (1920, 1080));
        assert_eq!(test_pattern_size(AspectRatio::Portrait9x16), (1080, 1920));
        assert_eq!(test_pattern_size(AspectRatio::Square1x1), (1080, 1080));
    }

    #[test]
    fn surface_format_avoids_srgb() {
        use wgpu::TextureFormat as F;
        assert_eq!(
            pick_surface_format(&[F::Bgra8UnormSrgb, F::Bgra8Unorm]),
            Some(F::Bgra8Unorm)
        );
        assert_eq!(
            pick_surface_format(&[F::Rgba8UnormSrgb, F::Rgba16Float]),
            Some(F::Rgba16Float)
        );
        assert_eq!(pick_surface_format(&[F::Bgra8UnormSrgb]), None);
    }
}
