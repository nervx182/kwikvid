//! The title card (MVP item 9): a section's [`TitleCard`] text rasterised
//! once per export into a transparent RGBA layer at the output size, which
//! the frame plan overlays on the opening frames ([`crate::OverlayId::Title`]).
//!
//! Look (all guesses at Quik's): white, centred, bold sans (the bundled Fira
//! Sans Bold, `assets/fonts/`, SIL OFL 1.1), [`TITLE_HEIGHT`] of the output
//! height, shrunk to fit [`TITLE_MAX_WIDTH`] of the width, one line, with a
//! soft dark drop shadow so it reads over bright footage.

use ab_glyph::{point, Font, FontRef, GlyphId, PxScale, ScaleFont};
use kv_core::{Micros, TitleCard};
use kv_media::VideoFrame;

/// The bundled title font (Fira Sans Bold; licence in `assets/fonts/`).
const FONT: &[u8] = include_bytes!("../../../assets/fonts/FiraSans-Bold.ttf");

/// Font size (em height in pixels) as a fraction of the output height
/// (guess: 8 %).
pub const TITLE_HEIGHT: f32 = 0.08;

/// Widest the text (shadow included) may be, as a fraction of the output
/// width; longer titles are drawn smaller.
pub const TITLE_MAX_WIDTH: f32 = 0.9;

/// Shadow offset down and right, as a fraction of the font size.
const SHADOW_OFFSET: f32 = 0.05;
/// Shadow blur radius, as a fraction of the font size.
const SHADOW_BLUR: f32 = 0.08;
/// Peak shadow opacity.
const SHADOW_ALPHA: f32 = 0.6;

/// Draw `title` as a straight-alpha RGBA frame of `out_w x out_h`: white text
/// over a soft black shadow, centred, on a fully transparent background
/// (alpha 0, colour 0). Text is one line (control characters such as line
/// breaks become spaces); characters the font lacks are skipped. An empty
/// output size gives an empty frame.
pub fn rasterize(title: &TitleCard, out_w: u32, out_h: u32) -> VideoFrame {
    let (w, h) = (out_w as usize, out_h as usize);
    let mut frame = VideoFrame::solid(Micros(0), out_w, out_h, [0, 0, 0, 0]);
    if w == 0 || h == 0 {
        return frame;
    }
    let font = FontRef::try_from_slice(FONT).expect("bundled font parses");
    let text: String = title
        .text
        .trim()
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();

    // Lay out at the nominal size; if the ink (plus shadow) is too wide,
    // scale down and lay out again. Outlines do not scale exactly linearly
    // once rounded to pixels, so repeat a few times with a 1 % margin.
    let max_w = TITLE_MAX_WIDTH * out_w as f32;
    let mut px = (TITLE_HEIGHT * out_h as f32).max(1.0);
    let mut line = Line::layout(&font, &text, px);
    for _ in 0..4 {
        let width = line.ink_width() + shadow_extent(px);
        if width <= max_w {
            break;
        }
        px *= max_w / width * 0.99;
        line = Line::layout(&font, &text, px);
    }

    // Centre: horizontally on the ink, vertically on the font's line box
    // (ascent to descent) so a title's baseline does not jump with its
    // letters.
    let scaled = font.as_scaled(PxScale::from(px));
    let (ascent, descent) = (scaled.ascent(), scaled.descent());
    let dx = out_w as f32 / 2.0 - (line.ink_min + line.ink_max) / 2.0;
    let baseline = out_h as f32 / 2.0 + (ascent + descent) / 2.0;

    let mut text_cov = vec![0f32; w * h];
    for &(id, x) in &line.glyphs {
        let glyph = id.with_scale_and_position(PxScale::from(px), point(x + dx, baseline));
        let Some(outline) = font.outline_glyph(glyph) else {
            continue;
        };
        let b = outline.px_bounds();
        let (bx, by) = (b.min.x as i64, b.min.y as i64);
        outline.draw(|gx, gy, c| {
            let (x, y) = (bx + i64::from(gx), by + i64::from(gy));
            if (0..w as i64).contains(&x) && (0..h as i64).contains(&y) {
                let v = &mut text_cov[y as usize * w + x as usize];
                *v = (*v + c).min(1.0);
            }
        });
    }

    let offset = (SHADOW_OFFSET * px).round() as i64;
    let radius = (SHADOW_BLUR * px).round().max(1.0) as usize;
    let mut shadow = shifted(&text_cov, w, h, offset);
    // Two box blurs approximate a Gaussian.
    for _ in 0..2 {
        box_blur(&mut shadow, w, h, radius);
    }

    // Straight-alpha "over": text (white) over shadow (black).
    for ((px_out, &t), &s) in frame.data.chunks_exact_mut(4).zip(&text_cov).zip(&shadow) {
        let s = s * SHADOW_ALPHA;
        let a = t + s * (1.0 - t);
        if a <= 0.0 {
            continue;
        }
        let v = (255.0 * t / a + 0.5) as u8;
        px_out.copy_from_slice(&[v, v, v, (255.0 * a + 0.5) as u8]);
    }
    frame
}

/// How far the shadow reaches beyond the text's ink, in pixels.
fn shadow_extent(px: f32) -> f32 {
    (SHADOW_OFFSET + 2.0 * SHADOW_BLUR) * px
}

/// One line of glyphs at a pen position, with its horizontal ink extent.
struct Line {
    /// Glyph and pen x (relative to the line start).
    glyphs: Vec<(GlyphId, f32)>,
    ink_min: f32,
    ink_max: f32,
}

impl Line {
    fn layout(font: &FontRef<'_>, text: &str, px: f32) -> Line {
        let scaled = font.as_scaled(PxScale::from(px));
        let mut glyphs = Vec::with_capacity(text.len());
        let mut pen = 0.0f32;
        let mut prev: Option<GlyphId> = None;
        let (mut ink_min, mut ink_max) = (f32::INFINITY, f32::NEG_INFINITY);
        for c in text.chars() {
            let id = font.glyph_id(c);
            if id.0 == 0 {
                // Not in the font (emoji, say): skip rather than draw a box.
                continue;
            }
            if let Some(p) = prev {
                pen += scaled.kern(p, id);
            }
            let glyph = id.with_scale_and_position(PxScale::from(px), point(pen, 0.0));
            if let Some(outline) = font.outline_glyph(glyph) {
                let b = outline.px_bounds();
                ink_min = ink_min.min(b.min.x);
                ink_max = ink_max.max(b.max.x);
            }
            glyphs.push((id, pen));
            pen += scaled.h_advance(id);
            prev = Some(id);
        }
        if ink_min > ink_max {
            (ink_min, ink_max) = (0.0, 0.0);
        }
        Line {
            glyphs,
            ink_min,
            ink_max,
        }
    }

    fn ink_width(&self) -> f32 {
        self.ink_max - self.ink_min
    }
}

/// `src` moved `d` pixels right and down; uncovered pixels are 0.
fn shifted(src: &[f32], w: usize, h: usize, d: i64) -> Vec<f32> {
    let mut out = vec![0f32; w * h];
    let d = d.max(0) as usize;
    for y in d..h {
        let from = &src[(y - d) * w..(y - d) * w + (w - d.min(w))];
        out[y * w + d.min(w)..(y + 1) * w].copy_from_slice(from);
    }
    out
}

/// In-place box blur of radius `r` (window `2r + 1`), rows then columns,
/// treating pixels outside the frame as 0.
fn box_blur(buf: &mut [f32], w: usize, h: usize, r: usize) {
    let mut line = Vec::new();
    for y in 0..h {
        line.clear();
        line.extend_from_slice(&buf[y * w..(y + 1) * w]);
        blur_line(&line, r, |i, v| buf[y * w + i] = v);
    }
    for x in 0..w {
        line.clear();
        line.extend((0..h).map(|y| buf[y * w + x]));
        blur_line(&line, r, |i, v| buf[i * w + x] = v);
    }
}

fn blur_line(src: &[f32], r: usize, mut put: impl FnMut(usize, f32)) {
    let n = src.len();
    let norm = 1.0 / (2 * r + 1) as f32;
    let mut sum: f32 = src[..r.min(n)].iter().sum();
    for i in 0..n {
        if i + r < n {
            sum += src[i + r];
        }
        if i > r {
            sum -= src[i - r - 1];
        }
        put(i, (sum * norm).max(0.0));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn card(text: &str) -> TitleCard {
        TitleCard {
            text: text.into(),
            duration: Micros(2_000_000),
        }
    }

    fn alpha(f: &VideoFrame, x: u32, y: u32) -> u8 {
        f.data[(y as usize * f.width as usize + x as usize) * 4 + 3]
    }

    /// Columns `[min, max]` holding any non-zero alpha.
    fn alpha_columns(f: &VideoFrame) -> Option<(u32, u32)> {
        let cols: Vec<u32> = (0..f.width)
            .filter(|&x| (0..f.height).any(|y| alpha(f, x, y) > 0))
            .collect();
        Some((*cols.first()?, *cols.last()?))
    }

    #[test]
    fn draws_opaque_white_text_in_the_centre_on_a_transparent_background() {
        let f = rasterize(&card("HELLO"), 640, 360);
        assert_eq!((f.width, f.height), (640, 360));
        for (x, y) in [(0, 0), (639, 0), (0, 359), (639, 359), (20, 180), (320, 20)] {
            assert_eq!(alpha(&f, x, y), 0, "({x}, {y})");
        }
        // Around the centre: some fully opaque white text pixels.
        let mut white = 0;
        for y in 160..200u32 {
            for x in 220..420u32 {
                let i = (y as usize * 640 + x as usize) * 4;
                if f.data[i..i + 4] == [255, 255, 255, 255] {
                    white += 1;
                }
            }
        }
        assert!(white > 200, "{white} white pixels near the centre");
        // Straight alpha: fully transparent pixels carry no colour.
        assert!(f
            .data
            .chunks_exact(4)
            .all(|p| p[3] > 0 || p == [0, 0, 0, 0]));
        // A shadow: semi-transparent dark pixels below-right of the text.
        assert!(f.data.chunks_exact(4).any(|p| p[3] > 20 && p[0] < 64));
    }

    #[test]
    fn text_is_centred_and_about_eight_percent_of_the_height() {
        let f = rasterize(&card("H"), 400, 1000);
        // Opaque rows of "H" (cap height ~0.69 em of an 80 px font).
        let rows: Vec<u32> = (0..1000)
            .filter(|&y| (0..400).any(|x| alpha(&f, x, y) == 255))
            .collect();
        let (top, bottom) = (rows[0], *rows.last().unwrap());
        let cap = bottom - top + 1;
        assert!((45..=65).contains(&cap), "cap height {cap}");
        let cols: Vec<u32> = (0..400)
            .filter(|&x| (0..1000).any(|y| alpha(&f, x, y) == 255))
            .collect();
        let cx = (cols[0] + cols.last().unwrap()) as f32 / 2.0;
        assert!((cx - 200.0).abs() <= 2.0, "centre x {cx}");
        let cy = (top + bottom) as f32 / 2.0;
        assert!((cy - 500.0).abs() <= 25.0, "centre y {cy}");
    }

    #[test]
    fn long_text_shrinks_to_fit_the_width() {
        // Capitals only, so text rows measure the cap height. At the
        // nominal size this would be ~760 px wide.
        let long = "HOLIDAY IN THE MOUNTAINS WITH ALL OUR FRIENDS AND FAMILY 2026";
        let f = rasterize(&card(long), 640, 360);
        let (x0, x1) = alpha_columns(&f).unwrap();
        // Everything (shadow included) inside the central 90 %.
        assert!(x0 >= 32 && x1 <= 608, "columns {x0}..={x1}");
        // And it really was shrunk: it fills most of that width.
        assert!(x1 - x0 > 480, "columns {x0}..={x1}");
        let short = rasterize(&card("HI"), 640, 360);
        // Rows where the text itself (mostly opaque) shows: shorter.
        let text_rows = |f: &VideoFrame| {
            (0..f.height)
                .filter(|&y| (0..f.width).any(|x| alpha(f, x, y) > 200))
                .count()
        };
        let (long_rows, short_rows) = (text_rows(&f), text_rows(&short));
        assert!(long_rows < short_rows, "{long_rows} vs {short_rows} rows");
    }

    #[test]
    fn control_characters_and_missing_glyphs_are_harmless() {
        let f = rasterize(&card("Day\none 🏄"), 320, 180);
        assert!(alpha_columns(&f).is_some());
        let only_emoji = rasterize(&card("🏄"), 64, 36);
        assert!(only_emoji.data.iter().all(|&b| b == 0));
        let empty = rasterize(&card("x"), 0, 0);
        assert!(empty.data.is_empty());
    }

    #[test]
    fn blur_and_shift_helpers() {
        let mut b = vec![0.0; 5];
        blur_line(&[0.0, 0.0, 3.0, 0.0, 0.0], 1, |i, v| b[i] = v);
        assert_eq!(b, [0.0, 1.0, 1.0, 1.0, 0.0]);
        // 2-D: a single 9 spreads to 1s over the surrounding 3x3 block.
        let mut g = vec![0.0; 25];
        g[12] = 9.0;
        box_blur(&mut g, 5, 5, 1);
        for (i, &v) in g.iter().enumerate() {
            let (x, y) = (i % 5, i / 5);
            let inside = (1..=3).contains(&x) && (1..=3).contains(&y);
            let want = if inside { 1.0 } else { 0.0 };
            assert!((v - want).abs() < 1e-6, "{i}: {v}");
        }
        let s = shifted(&[1.0, 2.0, 3.0, 4.0], 2, 2, 1);
        assert_eq!(s, [0.0, 0.0, 0.0, 1.0]);
    }
}
