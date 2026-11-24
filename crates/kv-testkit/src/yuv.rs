//! RGBA8 <-> 8-bit YUV 4:2:0 (BT.709, limited range), in plain Rust so the
//! video fixtures need no swscale.

/// An 8-bit planar YUV 4:2:0 picture with tightly packed planes: `y` is
/// `width * height`, `u` and `v` are `ceil(width/2) * ceil(height/2)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Yuv420p {
    pub width: u32,
    pub height: u32,
    pub y: Vec<u8>,
    pub u: Vec<u8>,
    pub v: Vec<u8>,
}

impl Yuv420p {
    /// Width and height of the chroma planes.
    pub fn chroma_size(&self) -> (usize, usize) {
        chroma_size(self.width, self.height)
    }
}

fn chroma_size(width: u32, height: u32) -> (usize, usize) {
    (width.div_ceil(2) as usize, height.div_ceil(2) as usize)
}

const KR: f64 = 0.2126;
const KB: f64 = 0.0722;
const KG: f64 = 1.0 - KR - KB;

fn to_u8(v: f64) -> u8 {
    v.round().clamp(0.0, 255.0) as u8
}

/// Convert an RGBA8 buffer (alpha ignored) to BT.709 limited-range YUV 4:2:0.
/// Chroma is taken from the mean colour of each 2x2 block.
///
/// # Panics
/// If `rgba.len() != width * height * 4`.
pub fn rgba_to_yuv420p(rgba: &[u8], width: u32, height: u32) -> Yuv420p {
    let (w, h) = (width as usize, height as usize);
    assert_eq!(
        rgba.len(),
        w * h * 4,
        "RGBA buffer does not match {width}x{height}"
    );
    let norm = |i: usize| {
        let p = &rgba[i * 4..i * 4 + 3];
        [p[0], p[1], p[2]].map(|c| f64::from(c) / 255.0)
    };
    let luma = |[r, g, b]: [f64; 3]| KR * r + KG * g + KB * b;

    let y = (0..w * h)
        .map(|i| to_u8(16.0 + 219.0 * luma(norm(i))))
        .collect();
    let (cw, ch) = chroma_size(width, height);
    let mut u = Vec::with_capacity(cw * ch);
    let mut v = Vec::with_capacity(cw * ch);
    for cy in 0..ch {
        for cx in 0..cw {
            let mut sum = [0.0; 3];
            let mut n = 0.0;
            for yy in 2 * cy..(2 * cy + 2).min(h) {
                for xx in 2 * cx..(2 * cx + 2).min(w) {
                    let p = norm(yy * w + xx);
                    for c in 0..3 {
                        sum[c] += p[c];
                    }
                    n += 1.0;
                }
            }
            let rgb = sum.map(|s| s / n);
            let l = luma(rgb);
            u.push(to_u8(128.0 + 224.0 * (rgb[2] - l) / (2.0 * (1.0 - KB))));
            v.push(to_u8(128.0 + 224.0 * (rgb[0] - l) / (2.0 * (1.0 - KR))));
        }
    }
    Yuv420p {
        width,
        height,
        y,
        u,
        v,
    }
}

/// Convert BT.709 limited-range YUV 4:2:0 to RGBA8 (alpha 255).
///
/// # Panics
/// If a plane's length does not match the picture size.
pub fn yuv420p_to_rgba(yuv: &Yuv420p) -> Vec<u8> {
    let (w, h) = (yuv.width as usize, yuv.height as usize);
    let (cw, ch) = yuv.chroma_size();
    assert_eq!(yuv.y.len(), w * h, "Y plane size");
    assert_eq!(yuv.u.len(), cw * ch, "U plane size");
    assert_eq!(yuv.v.len(), cw * ch, "V plane size");
    let mut out = Vec::with_capacity(w * h * 4);
    for row in 0..h {
        for col in 0..w {
            let c = (row / 2) * cw + col / 2;
            let l = (f64::from(yuv.y[row * w + col]) - 16.0) / 219.0;
            let pb = (f64::from(yuv.u[c]) - 128.0) / 224.0;
            let pr = (f64::from(yuv.v[c]) - 128.0) / 224.0;
            let r = l + 2.0 * (1.0 - KR) * pr;
            let b = l + 2.0 * (1.0 - KB) * pb;
            let g = (l - KR * r - KB * b) / KG;
            out.extend([to_u8(r * 255.0), to_u8(g * 255.0), to_u8(b * 255.0), 255]);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{clip_colour, read_frame_tag, render_tagged_frame, FrameTag};

    #[test]
    fn flat_colours_round_trip_within_two_levels() {
        let colours: Vec<[u8; 3]> = (0..12)
            .map(clip_colour)
            .chain([[0, 0, 0], [255, 255, 255], [128, 128, 128]])
            .collect();
        for [r, g, b] in colours {
            let rgba = [r, g, b, 255].repeat(6 * 4);
            let yuv = rgba_to_yuv420p(&rgba, 6, 4);
            let back = yuv420p_to_rgba(&yuv);
            for (a, b) in rgba.iter().zip(&back) {
                assert!(a.abs_diff(*b) <= 2, "{r},{g},{b}: {a} vs {b}");
            }
        }
    }

    #[test]
    fn black_and_white_use_limited_range() {
        let yuv = rgba_to_yuv420p(&[0, 0, 0, 255, 255, 255, 255, 255], 2, 1);
        assert_eq!(yuv.y, [16, 235]);
        assert_eq!(
            (yuv.u.as_slice(), yuv.v.as_slice()),
            (&[128][..], &[128][..])
        );
    }

    #[test]
    fn tagged_frame_survives_chroma_subsampling_at_odd_size() {
        let t = FrameTag {
            clip: 9,
            frame: 321,
        };
        let (w, h) = (321, 241);
        let yuv = rgba_to_yuv420p(&render_tagged_frame(t, w, h), w, h);
        assert_eq!(yuv.chroma_size(), (161, 121));
        assert_eq!(read_frame_tag(&yuv420p_to_rgba(&yuv), w, h), Some(t));
    }
}
