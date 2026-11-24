//! Machine-readable frame tags: which clip and which frame a picture came from.
//!
//! A tagged frame is a flat background in a hue picked by the clip index
//! ([`clip_colour`], 12 distinct hues) with a square code centred on it:
//!
//! ```text
//!   8 x 8 cells, side = 60 % of the shorter frame side
//!   +--------+
//!   |WWWWWWWW|   W = white ring (used to find the code)
//!   |W......W|   . = 36 data cells, black = 0, white = 1, row-major:
//!   |W......W|       bits 0..8   clip
//!   |W......W|       bits 8..24  frame
//!   |W......W|       bits 24..36 check bits (hash of bits 0..24)
//!   |W......W|
//!   |W......W|
//!   |WWWWWWWW|
//!   +--------+
//! ```
//!
//! The reader samples the background near the left edge, finds the white ring
//! by scanning the centre row and column from the outside in, and samples the
//! middle of every cell. Cells are large, so the code survives H.264 / MPEG-4
//! compression, scaling to any size of at least 160x90 and a centre crop of up
//! to 25 % of either side (an aspect change), since the code is sized from the
//! shorter side and located, not assumed. A cell that is neither clearly black
//! nor clearly white (as in a 50/50 crossfade of two different tags) makes the
//! whole read fail, as do wrong check bits or a background that does not match
//! the clip's hue.

/// Identifies one frame of one synthetic clip.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FrameTag {
    /// Clip index; picks the background hue (`clip % 12`).
    pub clip: u8,
    /// Frame index within the clip, from 0.
    pub frame: u16,
}

/// Number of distinct background hues.
pub const PALETTE_LEN: usize = 12;

/// Cells per side of the code, including the white ring.
const GRID: usize = 8;
/// Data cells per side.
const DATA: usize = GRID - 2;
/// Side of the code as a fraction of the shorter frame side.
const GRID_FRACTION: f64 = 0.6;
const PAYLOAD_BITS: u32 = 24;
const CHECK_BITS: u32 = (DATA * DATA) as u32 - PAYLOAD_BITS;

/// Luma at or below this is a black cell; at or above [`WHITE_MIN`] a white
/// one; anything in between is ambiguous.
const BLACK_MAX: f64 = 85.0;
const WHITE_MIN: f64 = 170.0;
/// Largest RGB distance between the sampled background and the clip's hue.
const BACKGROUND_TOLERANCE: f64 = 45.0;

/// Background colour (RGB8) of clip `clip`: 12 hues 30 degrees apart, all
/// saturated and mid-dark so the white ring and black cells stand out.
pub fn clip_colour(clip: u8) -> [u8; 3] {
    let hue = (usize::from(clip) % PALETTE_LEN) as f64 * 360.0 / PALETTE_LEN as f64;
    hsv_to_rgb(hue, 0.85, 0.65)
}

fn hsv_to_rgb(h: f64, s: f64, v: f64) -> [u8; 3] {
    let c = v * s;
    let hp = h / 60.0;
    let x = c * (1.0 - (hp % 2.0 - 1.0).abs());
    let (r, g, b) = match hp as u32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    let m = v - c;
    let to8 = |f: f64| ((f + m) * 255.0).round() as u8;
    [to8(r), to8(g), to8(b)]
}

fn check_bits(payload: u32) -> u32 {
    // Top bits of a multiplicative hash: every payload bit affects them.
    payload.wrapping_mul(0x9E37_79B1) >> (32 - CHECK_BITS)
}

fn encode(tag: FrameTag) -> u64 {
    let payload = u32::from(tag.clip) | (u32::from(tag.frame) << 8);
    u64::from(payload) | (u64::from(check_bits(payload)) << PAYLOAD_BITS)
}

fn decode(code: u64) -> Option<FrameTag> {
    let payload = (code & ((1 << PAYLOAD_BITS) - 1)) as u32;
    if u64::from(check_bits(payload)) != code >> PAYLOAD_BITS {
        return None;
    }
    Some(FrameTag {
        clip: (payload & 0xFF) as u8,
        frame: (payload >> 8) as u16,
    })
}

/// Draw `tag` into a new `width` x `height` RGBA8 buffer (row-major, no
/// padding, alpha 255).
///
/// # Panics
/// If `width` or `height` is below 16.
pub fn render_tagged_frame(tag: FrameTag, width: u32, height: u32) -> Vec<u8> {
    assert!(
        width >= 16 && height >= 16,
        "tagged frames need at least 16x16 pixels, got {width}x{height}"
    );
    let (w, h) = (width as usize, height as usize);
    let [r, g, b] = clip_colour(tag.clip);
    let mut out = [r, g, b, 255].repeat(w * h);

    // Whole-pixel cells keep edges crisp for the encoder.
    let cell = ((w.min(h) as f64 * GRID_FRACTION) as usize / GRID).max(1);
    let side = cell * GRID;
    let (x0, y0) = ((w - side) / 2, (h - side) / 2);
    let code = encode(tag);
    for gy in 0..GRID {
        for gx in 0..GRID {
            let ring = gx == 0 || gy == 0 || gx == GRID - 1 || gy == GRID - 1;
            let white = ring || {
                let bit = (gy - 1) * DATA + (gx - 1);
                code >> bit & 1 == 1
            };
            let v = if white { 255 } else { 0 };
            for y in y0 + gy * cell..y0 + (gy + 1) * cell {
                let row = &mut out[(y * w + x0 + gx * cell) * 4..][..cell * 4];
                for px in row.chunks_exact_mut(4) {
                    px[..3].fill(v);
                }
            }
        }
    }
    out
}

/// Read the tag drawn by [`render_tagged_frame`] from an RGBA8 buffer, after
/// any compression, scaling or centre crop it survives (see the module docs).
/// Returns `None` when no valid tag is found: random content, a blend of two
/// different tags, wrong check bits or a background that is not the clip's
/// hue.
///
/// # Panics
/// If `rgba.len() != width * height * 4`.
pub fn read_frame_tag(rgba: &[u8], width: u32, height: u32) -> Option<FrameTag> {
    let img = Rgba::new(rgba, width, height);
    if img.w < 16 || img.h < 16 {
        return None;
    }

    // Background: a patch near the left edge, which a 25 % crop keeps.
    let bg = img.mean_rgb(
        img.w as f64 * 0.02,
        img.h as f64 * 0.45,
        img.w as f64 * 0.06,
        img.h as f64 * 0.55,
    );
    let (hue, dist) = (0..PALETTE_LEN as u8)
        .map(|i| (i, rgb_distance(bg, clip_colour(i))))
        .min_by(|a, b| a.1.total_cmp(&b.1))?;
    if dist > BACKGROUND_TOLERANCE {
        return None;
    }

    // Ring: first pixel brighter than halfway between background and white,
    // scanning the centre row / column inwards from each edge.
    let edge = (luma(bg) + 255.0) / 2.0;
    let (cx, cy) = (img.w / 2, img.h / 2);
    let left = (0..cx).find(|&x| img.luma_at(x, cy) > edge)?;
    let right = (cx..img.w).rev().find(|&x| img.luma_at(x, cy) > edge)? + 1;
    let top = (0..cy).find(|&y| img.luma_at(cx, y) > edge)?;
    let bottom = (cy..img.h).rev().find(|&y| img.luma_at(cx, y) > edge)? + 1;
    let (gw, gh) = ((right - left) as f64, (bottom - top) as f64);
    if (gw - gh).abs() > 0.2 * gw.max(gh) {
        return None; // not square: not our code
    }
    let (cw, ch) = (gw / GRID as f64, gh / GRID as f64);
    if cw < 2.0 || ch < 2.0 {
        return None;
    }

    // Sample the middle 40 % of a cell.
    let cell_luma = |gx: usize, gy: usize| {
        let x = left as f64 + (gx as f64 + 0.3) * cw;
        let y = top as f64 + (gy as f64 + 0.3) * ch;
        luma(img.mean_rgb(x, y, x + 0.4 * cw, y + 0.4 * ch))
    };
    let mut code = 0u64;
    for gy in 0..GRID {
        for gx in 0..GRID {
            let l = cell_luma(gx, gy);
            let ring = gx == 0 || gy == 0 || gx == GRID - 1 || gy == GRID - 1;
            let white = if l >= WHITE_MIN {
                true
            } else if l <= BLACK_MAX {
                false
            } else {
                return None; // ambiguous cell: blend or noise
            };
            if ring {
                if !white {
                    return None;
                }
            } else if white {
                code |= 1 << ((gy - 1) * DATA + (gx - 1));
            }
        }
    }
    let tag = decode(code)?;
    (usize::from(tag.clip) % PALETTE_LEN == usize::from(hue)).then_some(tag)
}

fn luma([r, g, b]: [f64; 3]) -> f64 {
    0.2126 * r + 0.7152 * g + 0.0722 * b
}

fn rgb_distance(a: [f64; 3], b: [u8; 3]) -> f64 {
    a.iter()
        .zip(b)
        .map(|(x, y)| (x - f64::from(y)).powi(2))
        .sum::<f64>()
        .sqrt()
}

struct Rgba<'a> {
    data: &'a [u8],
    w: usize,
    h: usize,
}

impl<'a> Rgba<'a> {
    fn new(data: &'a [u8], width: u32, height: u32) -> Self {
        let (w, h) = (width as usize, height as usize);
        assert_eq!(
            data.len(),
            w * h * 4,
            "RGBA buffer length does not match {width}x{height}"
        );
        Self { data, w, h }
    }

    fn rgb_at(&self, x: usize, y: usize) -> [f64; 3] {
        let p = &self.data[(y * self.w + x) * 4..][..3];
        [f64::from(p[0]), f64::from(p[1]), f64::from(p[2])]
    }

    fn luma_at(&self, x: usize, y: usize) -> f64 {
        luma(self.rgb_at(x, y))
    }

    /// Mean colour of the pixels whose centres lie in `[x0, x1) x [y0, y1)`,
    /// or of the single pixel nearest the box centre if there are none.
    fn mean_rgb(&self, x0: f64, y0: f64, x1: f64, y1: f64) -> [f64; 3] {
        let clamp_x = |v: f64| (v.max(0.0) as usize).min(self.w - 1);
        let clamp_y = |v: f64| (v.max(0.0) as usize).min(self.h - 1);
        let (xa, xb) = (clamp_x((x0 - 0.5).ceil()), clamp_x((x1 - 0.5).ceil()));
        let (ya, yb) = (clamp_y((y0 - 0.5).ceil()), clamp_y((y1 - 0.5).ceil()));
        if xa >= xb || ya >= yb {
            return self.rgb_at(clamp_x((x0 + x1) / 2.0), clamp_y((y0 + y1) / 2.0));
        }
        let mut sum = [0.0; 3];
        for y in ya..yb {
            for x in xa..xb {
                let p = self.rgb_at(x, y);
                for c in 0..3 {
                    sum[c] += p[c];
                }
            }
        }
        let n = ((xb - xa) * (yb - ya)) as f64;
        sum.map(|s| s / n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FRAMES: [u16; 7] = [0, 1, 2, 59, 1000, 32_767, u16::MAX];

    fn tag(clip: u8, frame: u16) -> FrameTag {
        FrameTag { clip, frame }
    }

    /// Bilinear resample of an RGBA8 buffer.
    fn resize(src: &[u8], sw: usize, sh: usize, dw: usize, dh: usize) -> Vec<u8> {
        let mut out = vec![0u8; dw * dh * 4];
        for y in 0..dh {
            let fy = ((y as f64 + 0.5) * sh as f64 / dh as f64 - 0.5).clamp(0.0, (sh - 1) as f64);
            let (y0, ty) = (fy as usize, fy.fract());
            let y1 = (y0 + 1).min(sh - 1);
            for x in 0..dw {
                let fx =
                    ((x as f64 + 0.5) * sw as f64 / dw as f64 - 0.5).clamp(0.0, (sw - 1) as f64);
                let (x0, tx) = (fx as usize, fx.fract());
                let x1 = (x0 + 1).min(sw - 1);
                for c in 0..4 {
                    let p = |xx: usize, yy: usize| f64::from(src[(yy * sw + xx) * 4 + c]);
                    let top = p(x0, y0) * (1.0 - tx) + p(x1, y0) * tx;
                    let bot = p(x0, y1) * (1.0 - tx) + p(x1, y1) * tx;
                    out[(y * dw + x) * 4 + c] = (top * (1.0 - ty) + bot * ty).round() as u8;
                }
            }
        }
        out
    }

    /// Keep the centre `cw` x `ch` pixels.
    fn centre_crop(src: &[u8], sw: usize, sh: usize, cw: usize, ch: usize) -> Vec<u8> {
        let (x0, y0) = ((sw - cw) / 2, (sh - ch) / 2);
        (y0..y0 + ch)
            .flat_map(|y| {
                src[(y * sw + x0) * 4..(y * sw + x0 + cw) * 4]
                    .iter()
                    .copied()
            })
            .collect()
    }

    #[test]
    fn round_trip_every_palette_clip_at_320x240_and_1080p() {
        for (w, h) in [(320, 240), (1920, 1080)] {
            for clip in 0..13u8 {
                let frames: &[u16] = if w == 320 { &FRAMES } else { &FRAMES[..3] };
                for &frame in frames {
                    let t = tag(clip, frame);
                    let img = render_tagged_frame(t, w, h);
                    assert_eq!(img.len(), (w * h * 4) as usize);
                    assert_eq!(read_frame_tag(&img, w, h), Some(t), "{t:?} at {w}x{h}");
                }
            }
        }
    }

    #[test]
    fn round_trip_high_clip_indices_and_odd_sizes() {
        for (clip, (w, h)) in [(12, (160, 90)), (200, (161, 91)), (255, (240, 320))] {
            let t = tag(clip, 4242);
            assert_eq!(read_frame_tag(&render_tagged_frame(t, w, h), w, h), Some(t));
        }
    }

    #[test]
    fn palette_has_twelve_distinct_hues() {
        let colours: Vec<_> = (0..PALETTE_LEN as u8).map(clip_colour).collect();
        for (i, a) in colours.iter().enumerate() {
            let fa = a.map(f64::from);
            for b in &colours[i + 1..] {
                assert!(rgb_distance(fa, *b) > 2.0 * BACKGROUND_TOLERANCE * 0.75);
            }
            assert!(luma(fa) < 170.0, "background must be darker than white");
        }
        assert_eq!(clip_colour(0), clip_colour(12));
    }

    #[test]
    fn survives_scaling_and_centre_crop() {
        let t = tag(7, 1234);
        // 16:9 source cropped to 4:3, and 4:3 source cropped to 16:9 (25 %).
        for (sw, sh, cw, ch) in [(1920, 1080, 1440, 1080), (640, 480, 640, 360)] {
            let img = render_tagged_frame(t, sw as u32, sh as u32);
            let cropped = centre_crop(&img, sw, sh, cw, ch);
            for (dw, dh) in [(160, 90), (160, 120), (320, 180), (1280, 720)] {
                // Uniform scale so the cropped picture covers dw x dh, then
                // crop the overflow: what a "fill" project does.
                let s = (dw as f64 / cw as f64).max(dh as f64 / ch as f64);
                let (fw, fh) = (
                    ((cw as f64 * s).round() as usize).max(dw),
                    ((ch as f64 * s).round() as usize).max(dh),
                );
                let scaled = resize(&cropped, cw, ch, fw, fh);
                let out = centre_crop(&scaled, fw, fh, dw, dh);
                assert_eq!(
                    read_frame_tag(&out, dw as u32, dh as u32),
                    Some(t),
                    "{sw}x{sh} -> crop {cw}x{ch} -> {dw}x{dh}"
                );
            }
        }
        // Plain downscale of a 1080p frame to the minimum size.
        let img = render_tagged_frame(t, 1920, 1080);
        assert_eq!(
            read_frame_tag(&resize(&img, 1920, 1080, 160, 90), 160, 90),
            Some(t)
        );
    }

    #[test]
    fn blend_of_two_different_tags_is_none() {
        let pairs = [
            (tag(0, 0), tag(1, 0)),
            (tag(3, 10), tag(3, 11)),
            (tag(5, 100), tag(9, 100)),
            (tag(0, 0), tag(12, 0)), // same hue, different clip
        ];
        for (w, h) in [(320, 240), (1920, 1080)] {
            for (a, b) in pairs {
                let ia = render_tagged_frame(a, w, h);
                let ib = render_tagged_frame(b, w, h);
                let blend: Vec<u8> = ia
                    .iter()
                    .zip(&ib)
                    .map(|(&x, &y)| ((u16::from(x) + u16::from(y)) / 2) as u8)
                    .collect();
                assert_eq!(read_frame_tag(&blend, w, h), None, "{a:?} + {b:?}");
            }
        }
    }

    #[test]
    fn noise_and_flat_frames_are_none() {
        let (w, h) = (320u32, 240u32);
        let mut state = 0x1234_5678_9abc_def0u64;
        for _ in 0..20 {
            let noise: Vec<u8> = (0..w * h * 4)
                .map(|_| {
                    // xorshift64*
                    state ^= state >> 12;
                    state ^= state << 25;
                    state ^= state >> 27;
                    (state.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 56) as u8
                })
                .collect();
            assert_eq!(read_frame_tag(&noise, w, h), None);
        }
        for v in [0u8, 128, 255] {
            assert_eq!(read_frame_tag(&vec![v; (w * h * 4) as usize], w, h), None);
        }
        let [r, g, b] = clip_colour(4);
        assert_eq!(
            read_frame_tag(&[r, g, b, 255].repeat((w * h) as usize), w, h),
            None
        );
    }

    #[test]
    fn wrong_check_bits_are_rejected() {
        let code = encode(tag(3, 77));
        assert_eq!(decode(code), Some(tag(3, 77)));
        for bit in 0..(DATA * DATA) {
            assert_eq!(decode(code ^ (1 << bit)), None, "flipped bit {bit}");
        }
    }
}
