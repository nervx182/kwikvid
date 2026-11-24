//! Synthetic "decoded frame": scrolling colour bars, a moving box and a big
//! frame counter, rendered on the CPU into an RGBA buffer. Every preview path
//! uses the same generator so the comparison is about delivery, not content.

const BARS: [[u8; 4]; 8] = [
    [235, 235, 235, 255],
    [235, 235, 16, 255],
    [16, 235, 235, 255],
    [16, 235, 16, 255],
    [235, 16, 235, 255],
    [235, 16, 16, 255],
    [16, 16, 235, 255],
    [40, 40, 40, 255],
];

/// Renders frame `n` at `w`x`h` into `out` (resized to `w * h * 4` bytes).
pub fn render(n: u64, w: u32, h: u32, out: &mut Vec<u8>) {
    let (w, h) = (w as usize, h as usize);
    let stride = w * 4;
    out.resize(stride * h, 0);

    // One row of scrolling bars, copied into every row.
    let bar_w = (w / 8).max(1);
    let shift = (n as usize * 4) % w;
    let mut row = vec![0u8; stride];
    for (x, px) in row.chunks_exact_mut(4).enumerate() {
        px.copy_from_slice(&BARS[((x + shift) / bar_w) % 8]);
    }
    for r in out.chunks_exact_mut(stride) {
        r.copy_from_slice(&row);
    }

    // Box sweeping left to right across the middle third.
    let box_w = w / 10;
    let travel = w.saturating_sub(box_w).max(1);
    let phase = (n as usize * 12) % (2 * travel);
    let bx = if phase < travel {
        phase
    } else {
        2 * travel - phase
    };
    fill(out, stride, bx, h / 3, box_w, h / 3, [0, 0, 0, 255]);
    fill(
        out,
        stride,
        bx + box_w / 4,
        h / 3 + h / 12,
        box_w / 2,
        h / 6,
        [255, 255, 255, 255],
    );

    // Frame counter, 7-segment style, top-left on a black plate.
    let digits = format!("{:06}", n % 1_000_000);
    let s = (h / 54).max(2); // segment thickness
    let (dw, dh, gap) = (4 * s, 7 * s, 2 * s);
    let plate_w = digits.len() * (dw + gap) + gap;
    fill(out, stride, 0, 0, plate_w, dh + 2 * gap, [0, 0, 0, 255]);
    for (i, c) in digits.bytes().enumerate() {
        let x0 = gap + i * (dw + gap);
        draw_digit(out, stride, x0, gap, s, c - b'0');
    }
}

fn fill(out: &mut [u8], stride: usize, x: usize, y: usize, w: usize, h: usize, c: [u8; 4]) {
    let width = stride / 4;
    let height = out.len() / stride;
    let x1 = (x + w).min(width);
    let y1 = (y + h).min(height);
    if x >= x1 || y >= y1 {
        return;
    }
    for r in y..y1 {
        let row = &mut out[r * stride + x * 4..r * stride + x1 * 4];
        for px in row.chunks_exact_mut(4) {
            px.copy_from_slice(&c);
        }
    }
}

fn draw_digit(out: &mut [u8], stride: usize, x: usize, y: usize, s: usize, d: u8) {
    // Segments: a b c d e f g (top, top-right, bottom-right, bottom, bottom-left, top-left, middle).
    const SEG: [u8; 10] = [
        0b0111111, 0b0000110, 0b1011011, 0b1001111, 0b1100110, 0b1101101, 0b1111101, 0b0000111,
        0b1111111, 0b1101111,
    ];
    let on = SEG[d as usize % 10];
    let white = [255, 255, 255, 255];
    let (w, h) = (4 * s, 7 * s);
    let rects = [
        (x, y, w, s),                 // a
        (x + w - s, y, s, h / 2 + 1), // b
        (x + w - s, y + h / 2, s, h / 2),
        (x, y + h - s, w, s),     // d
        (x, y + h / 2, s, h / 2), // e
        (x, y, s, h / 2 + 1),     // f
        (x, y + h / 2 - s / 2, w, s),
    ];
    for (i, (rx, ry, rw, rh)) in rects.into_iter().enumerate() {
        if on & (1 << i) != 0 {
            fill(out, stride, rx, ry, rw, rh, white);
        }
    }
}

/// Drops the alpha channel (for encoders that want RGB).
pub fn rgba_to_rgb(rgba: &[u8]) -> Vec<u8> {
    let mut rgb = Vec::with_capacity(rgba.len() / 4 * 3);
    for px in rgba.chunks_exact(4) {
        rgb.extend_from_slice(&px[..3]);
    }
    rgb
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_differ_and_have_right_size() {
        let (mut a, mut b) = (Vec::new(), Vec::new());
        render(1, 640, 360, &mut a);
        render(2, 640, 360, &mut b);
        assert_eq!(a.len(), 640 * 360 * 4);
        assert_ne!(a, b);
    }
}
