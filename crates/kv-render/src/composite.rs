//! CPU compositor for export (ADR 0009): scales each source layer to the
//! output with [`Fit`], then blends the layers by weight.
//!
//! Choices, all deliberate for M1:
//!
//! - **Bilinear** resampling with pixel-centre alignment and edge clamping.
//!   Good for upscaling and mild downscaling; large downscales alias, which
//!   is why the pipeline asks the decoder for a pre-scaled source with
//!   [`source_size_for`] and leaves the compositor a crop or a copy.
//! - Arithmetic is in **gamma-encoded sRGB** 8-bit values (no linearisation),
//!   accumulated in `f32` and rounded once. A crossfade therefore dips
//!   slightly darker mid-way than a linear-light blend would; Quik-style
//!   crossfades are short and this matches what most editors do in 8-bit.
//! - Areas a layer does not cover (letterbox bars) count as **opaque black**
//!   for that layer, so `out = sum(weight_i * layer_i)` holds everywhere.
//! - A layer at the output size with weight 1 is an exact copy (bit for bit).
//! - Rows are split across `std::thread::scope` workers (no extra
//!   dependency).

use kv_media::{PixelFormat, VideoFrame};

/// How a source of another aspect ratio is placed in the output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Fit {
    /// Scale to cover the whole output, centre crop the overflow (the Quik
    /// default, guess).
    #[default]
    Fill,
    /// Scale to fit inside the output, centred, with black bars.
    Fit,
}

/// Errors from [`composite`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CompositeError {
    /// A frame's `data` length does not match `width * height * 4`.
    #[error("{which} frame is {width}x{height} but holds {len} bytes")]
    InvalidFrame {
        which: &'static str,
        width: u32,
        height: u32,
        len: usize,
    },
}

/// The size a `src_w x src_h` source is scaled to when placed in an
/// `out_w x out_h` output with `fit`.
///
/// The pipeline asks the decoder for this size, so [`composite`] only has to
/// crop (`Fill`) or pad (`Fit`). One side equals the output side exactly;
/// the other is rounded to the nearest pixel and is `>=` (`Fill`) or `<=`
/// (`Fit`) the output side. Idempotent: feeding the result back as the
/// source returns it unchanged. Returns `(0, 0)` if any input is zero.
pub fn source_size_for(out_w: u32, out_h: u32, src_w: u32, src_h: u32, fit: Fit) -> (u32, u32) {
    if out_w == 0 || out_h == 0 || src_w == 0 || src_h == 0 {
        return (0, 0);
    }
    let (ow, oh, sw, sh) = (
        u64::from(out_w),
        u64::from(out_h),
        u64::from(src_w),
        u64::from(src_h),
    );
    // `ow * sh` vs `oh * sw` compares the aspect ratios exactly.
    let width_bound = match fit {
        // Output relatively wider than the source: match widths, crop height.
        Fit::Fill => ow * sh >= oh * sw,
        // Source relatively wider than the output: match widths, pad height.
        Fit::Fit => ow * sh <= oh * sw,
    };
    let round_div = |a: u64, b: u64| -> u32 {
        let v = (2 * a + b) / (2 * b);
        u32::try_from(v.max(1)).unwrap_or(u32::MAX)
    };
    if width_bound {
        (out_w, round_div(sh * ow, sw))
    } else {
        (round_div(sw * oh, sh), out_h)
    }
}

/// Draw `layers` (frame, weight) into `out` with `fit`, overwriting it.
///
/// `out`'s size and `pts` are kept; its pixels become
/// `sum(weight_i * fitted(layer_i))`, rounded and clamped to `0..=255`, where
/// a fitted layer is opaque black outside the area it covers. No layers
/// gives opaque black. Weights come from the frame plan and normally sum to
/// 1; they are used as given.
pub fn composite(
    layers: &[(&VideoFrame, f32)],
    out: &mut VideoFrame,
    fit: Fit,
) -> Result<(), CompositeError> {
    check_frame("output", out)?;
    for (frame, _) in layers {
        check_frame("layer", frame)?;
    }
    let (out_w, out_h) = (out.width as usize, out.height as usize);
    if out_w == 0 || out_h == 0 {
        return Ok(());
    }
    let prepared: Vec<Prepared<'_>> = layers
        .iter()
        .map(|&(frame, weight)| Prepared::new(frame, weight, out.width, out.height, fit))
        .collect();

    let stride = out_w * 4;
    let threads = std::thread::available_parallelism()
        .map_or(1, |n| n.get())
        .min(out_h);
    // Small frames are not worth a thread spawn.
    let threads = if out_w * out_h < 64 * 1024 {
        1
    } else {
        threads
    };
    let rows_per_chunk = out_h.div_ceil(threads);
    if threads == 1 {
        render_rows(&prepared, &mut out.data, 0, out_w);
    } else {
        std::thread::scope(|s| {
            for (i, chunk) in out.data.chunks_mut(rows_per_chunk * stride).enumerate() {
                let prepared = &prepared;
                s.spawn(move || render_rows(prepared, chunk, i * rows_per_chunk, out_w));
            }
        });
    }
    Ok(())
}

fn check_frame(which: &'static str, f: &VideoFrame) -> Result<(), CompositeError> {
    let PixelFormat::Rgba8 = f.format;
    let expected = (f.width as usize)
        .checked_mul(f.height as usize)
        .and_then(|n| n.checked_mul(4));
    if expected != Some(f.data.len()) {
        return Err(CompositeError::InvalidFrame {
            which,
            width: f.width,
            height: f.height,
            len: f.data.len(),
        });
    }
    Ok(())
}

/// Bilinear taps for one output coordinate: `v = (1 - t) * s[i0] + t * s[i1]`.
#[derive(Debug, Clone, Copy)]
struct Tap {
    i0: usize,
    i1: usize,
    t: f32,
}

/// Taps for the output coordinates `[start, end)` of an axis where the
/// scaled source spans `[offset, offset + scaled)` and has `src` pixels.
fn taps(start: usize, end: usize, offset: i64, scaled: u32, src: u32) -> Vec<Tap> {
    let ratio = f64::from(src) / f64::from(scaled);
    let max = f64::from(src - 1);
    (start..end)
        .map(|o| {
            let u = o as i64 - offset;
            // Pixel-centre alignment; exact integers when `src == scaled`.
            let s = ((u as f64 + 0.5) * ratio - 0.5).clamp(0.0, max);
            let i0 = s.floor() as usize;
            let i1 = (i0 + 1).min(src as usize - 1);
            Tap {
                i0,
                i1,
                t: (s - i0 as f64) as f32,
            }
        })
        .collect()
}

/// Where a layer lands in the output: the scaled size, its (centred)
/// offset, and the covered output rectangle `[x0, x1) x [y0, y1)`. Shared
/// with the GPU compositor so both place layers identically.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Placement {
    pub sw: u32,
    pub sh: u32,
    pub off_x: i64,
    pub off_y: i64,
    pub x0: usize,
    pub x1: usize,
    pub y0: usize,
    pub y1: usize,
}

impl Placement {
    pub(crate) fn new(src_w: u32, src_h: u32, out_w: u32, out_h: u32, fit: Fit) -> Self {
        let (sw, sh) = source_size_for(out_w, out_h, src_w, src_h, fit);
        if sw == 0 || sh == 0 {
            return Placement {
                sw,
                sh,
                off_x: 0,
                off_y: 0,
                x0: 0,
                x1: 0,
                y0: 0,
                y1: 0,
            };
        }
        // Centre; with an odd difference the extra pixel goes right / bottom.
        let off_x = (i64::from(out_w) - i64::from(sw)).div_euclid(2);
        let off_y = (i64::from(out_h) - i64::from(sh)).div_euclid(2);
        let span = |off: i64, len: u32, out: u32| {
            let a = off.clamp(0, i64::from(out)) as usize;
            let b = (off + i64::from(len)).clamp(0, i64::from(out)) as usize;
            (a, b)
        };
        let (x0, x1) = span(off_x, sw, out_w);
        let (y0, y1) = span(off_y, sh, out_h);
        Placement {
            sw,
            sh,
            off_x,
            off_y,
            x0,
            x1,
            y0,
            y1,
        }
    }
}

/// One layer resolved against the output geometry.
struct Prepared<'a> {
    frame: &'a VideoFrame,
    weight: f32,
    /// Output columns `[x0, x1)` and rows `[y0, y1)` the layer covers.
    x0: usize,
    x1: usize,
    y0: usize,
    y1: usize,
    /// Per covered column / row taps (empty if the layer covers nothing).
    xs: Vec<Tap>,
    ys: Vec<Tap>,
    /// Source and placed size match: rows are plain copies of source rows.
    unscaled: bool,
}

impl<'a> Prepared<'a> {
    fn new(frame: &'a VideoFrame, weight: f32, out_w: u32, out_h: u32, fit: Fit) -> Self {
        let p = Placement::new(frame.width, frame.height, out_w, out_h, fit);
        if p.sw == 0 || p.sh == 0 {
            return Prepared {
                frame,
                weight,
                x0: 0,
                x1: 0,
                y0: 0,
                y1: 0,
                xs: Vec::new(),
                ys: Vec::new(),
                unscaled: false,
            };
        }
        Prepared {
            frame,
            weight,
            x0: p.x0,
            x1: p.x1,
            y0: p.y0,
            y1: p.y1,
            xs: taps(p.x0, p.x1, p.off_x, p.sw, frame.width),
            ys: taps(p.y0, p.y1, p.off_y, p.sh, frame.height),
            unscaled: p.sw == frame.width && p.sh == frame.height,
        }
    }

    /// Add this layer's contribution to output row `y` into `acc`
    /// (`out_w * 4` floats). `cache` holds this layer's horizontally
    /// resampled source rows, reused across output rows.
    fn accumulate_row(&self, y: usize, acc: &mut [f32], cache: &mut RowCache) {
        let w = self.weight;
        let covered = y >= self.y0 && y < self.y1 && self.x0 < self.x1;
        if !covered {
            add_black(acc, w);
            return;
        }
        add_black(&mut acc[..self.x0 * 4], w);
        add_black(&mut acc[self.x1 * 4..], w);
        let row_acc = &mut acc[self.x0 * 4..self.x1 * 4];
        let ty = self.ys[y - self.y0];
        if self.unscaled {
            let sx = self.xs[0].i0 * 4;
            let src = &self.frame.data[ty.i0 * self.frame.stride() + sx..][..row_acc.len()];
            for (a, &s) in row_acc.iter_mut().zip(src) {
                *a += w * f32::from(s);
            }
            return;
        }
        let (s0, s1) = cache.rows(self, ty.i0, ty.i1);
        let (w0, w1) = (w * (1.0 - ty.t), w * ty.t);
        for ((a, &h0), &h1) in row_acc.iter_mut().zip(s0).zip(s1) {
            *a += w0 * h0 + w1 * h1;
        }
    }

    /// Horizontally resample source row `sy` over the covered columns.
    fn resample_row(&self, sy: usize, dst: &mut [f32]) {
        let row = &self.frame.data[sy * self.frame.stride()..][..self.frame.stride()];
        for (d, tx) in dst.chunks_exact_mut(4).zip(&self.xs) {
            let p0: &[u8; 4] = row[tx.i0 * 4..][..4].try_into().expect("4 bytes");
            let p1: &[u8; 4] = row[tx.i1 * 4..][..4].try_into().expect("4 bytes");
            let (wx0, wx1) = (1.0 - tx.t, tx.t);
            for c in 0..4 {
                d[c] = wx0 * f32::from(p0[c]) + wx1 * f32::from(p1[c]);
            }
        }
    }
}

/// Two horizontally resampled source rows of one layer (per worker).
/// Upscaling reads each source row for several output rows in a row, so
/// most output rows only cost the vertical lerp.
#[derive(Default)]
struct RowCache {
    keys: [Option<usize>; 2],
    rows: [Vec<f32>; 2],
}

impl RowCache {
    fn find(&self, sy: usize) -> Option<usize> {
        self.keys.iter().position(|&k| k == Some(sy))
    }

    fn load(&mut self, layer: &Prepared<'_>, slot: usize, sy: usize) {
        self.rows[slot].resize((layer.x1 - layer.x0) * 4, 0.0);
        layer.resample_row(sy, &mut self.rows[slot]);
        self.keys[slot] = Some(sy);
    }

    /// The resampled rows `a` and `b`, loading whichever is missing without
    /// evicting the other.
    fn rows(&mut self, layer: &Prepared<'_>, a: usize, b: usize) -> (&[f32], &[f32]) {
        let ia = match self.find(a) {
            Some(i) => i,
            None => {
                let i = if self.find(b) == Some(0) { 1 } else { 0 };
                self.load(layer, i, a);
                i
            }
        };
        let ib = match self.find(b) {
            Some(i) => i,
            None => {
                self.load(layer, 1 - ia, b);
                1 - ia
            }
        };
        (&self.rows[ia], &self.rows[ib])
    }
}

/// Opaque black weighted by `w`: only alpha gets anything.
fn add_black(acc: &mut [f32], w: f32) {
    for px in acc.chunks_exact_mut(4) {
        px[3] += w * 255.0;
    }
}

/// Render the output rows starting at `first_row` into `chunk`.
fn render_rows(layers: &[Prepared<'_>], chunk: &mut [u8], first_row: usize, out_w: usize) {
    let stride = out_w * 4;
    // Fast path: the frame is a single exact copy (the common non-transition
    // frame once the decoder pre-scales).
    if let [only] = layers {
        if only.weight == 1.0 && only.unscaled && only.x0 == 0 && only.x1 == out_w {
            let sx = only.xs[0].i0 * 4;
            let src_stride = only.frame.stride();
            for (i, row) in chunk.chunks_exact_mut(stride).enumerate() {
                let y = first_row + i;
                if y >= only.y0 && y < only.y1 {
                    let sy = only.ys[y - only.y0].i0;
                    row.copy_from_slice(&only.frame.data[sy * src_stride + sx..][..stride]);
                } else {
                    fill_black(row);
                }
            }
            return;
        }
    }
    if layers.is_empty() {
        fill_black(chunk);
        return;
    }
    let mut acc = vec![0.0f32; stride];
    let mut caches: Vec<RowCache> = layers.iter().map(|_| RowCache::default()).collect();
    for (i, row) in chunk.chunks_exact_mut(stride).enumerate() {
        acc.fill(0.0);
        for (layer, cache) in layers.iter().zip(&mut caches) {
            layer.accumulate_row(first_row + i, &mut acc, cache);
        }
        for (o, &a) in row.iter_mut().zip(&acc) {
            // `as u8` saturates (and maps NaN to 0).
            *o = (a + 0.5) as u8;
        }
    }
}

fn fill_black(buf: &mut [u8]) {
    for px in buf.chunks_exact_mut(4) {
        px.copy_from_slice(&[0, 0, 0, 255]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kv_core::Micros;

    const RED: [u8; 4] = [255, 0, 0, 255];
    const GREEN: [u8; 4] = [0, 255, 0, 255];
    const BLUE: [u8; 4] = [0, 0, 255, 255];
    const BLACK: [u8; 4] = [0, 0, 0, 255];

    fn px(f: &VideoFrame, x: u32, y: u32) -> [u8; 4] {
        let i = (y as usize * f.width as usize + x as usize) * 4;
        f.data[i..i + 4].try_into().unwrap()
    }

    fn out(w: u32, h: u32) -> VideoFrame {
        // Garbage so tests prove every pixel is overwritten.
        VideoFrame::solid(Micros(7), w, h, [9, 99, 199, 3])
    }

    /// A frame where every pixel is distinct-ish, to catch any resampling.
    fn noise(w: u32, h: u32) -> VideoFrame {
        let mut s: u32 = 0x1234_5678;
        let data = (0..w * h * 4)
            .map(|_| {
                s ^= s << 13;
                s ^= s >> 17;
                s ^= s << 5;
                (s >> 24) as u8
            })
            .collect();
        VideoFrame::new_rgba(Micros(0), w, h, data).unwrap()
    }

    #[test]
    fn source_size_for_fill_and_fit() {
        // 4:3 into 16:9.
        assert_eq!(
            source_size_for(1920, 1080, 320, 240, Fit::Fill),
            (1920, 1440)
        );
        assert_eq!(
            source_size_for(1920, 1080, 320, 240, Fit::Fit),
            (1440, 1080)
        );
        // 4K 16:9 into 1080p: plain downscale either way.
        for fit in [Fit::Fill, Fit::Fit] {
            assert_eq!(source_size_for(1920, 1080, 3840, 2160, fit), (1920, 1080));
        }
        // 16:9 into 9:16.
        assert_eq!(
            source_size_for(1080, 1920, 1920, 1080, Fit::Fill),
            (3413, 1920)
        );
        assert_eq!(
            source_size_for(1080, 1920, 1920, 1080, Fit::Fit),
            (1080, 608)
        );
        // Zero anywhere.
        assert_eq!(source_size_for(0, 1080, 320, 240, Fit::Fill), (0, 0));
        assert_eq!(source_size_for(1920, 1080, 320, 0, Fit::Fit), (0, 0));
    }

    #[test]
    fn source_size_for_is_idempotent_and_bounded() {
        let sizes = [
            (1920, 1080),
            (1080, 1920),
            (640, 360),
            (320, 240),
            (4000, 3000),
            (2704, 1520),
            (1, 1),
            (7, 3),
            (5312, 2988),
        ];
        for &(ow, oh) in &sizes {
            for &(sw, sh) in &sizes {
                for fit in [Fit::Fill, Fit::Fit] {
                    let (w, h) = source_size_for(ow, oh, sw, sh, fit);
                    assert!(w == ow || h == oh, "{ow}x{oh} {sw}x{sh} {fit:?}");
                    match fit {
                        Fit::Fill => assert!(w >= ow && h >= oh),
                        Fit::Fit => assert!(w <= ow && h <= oh),
                    }
                    assert_eq!(source_size_for(ow, oh, w, h, fit), (w, h));
                }
            }
        }
    }

    #[test]
    fn no_layers_gives_black() {
        let mut o = out(64, 48);
        composite(&[], &mut o, Fit::Fill).unwrap();
        assert!(o.data.chunks_exact(4).all(|p| p == BLACK));
        assert_eq!(o.pts, Micros(7));
    }

    #[test]
    fn single_full_weight_layer_is_exact_copy() {
        for (w, h) in [(64, 48), (1920, 1080), (3, 1)] {
            let src = noise(w, h);
            for fit in [Fit::Fill, Fit::Fit] {
                let mut o = out(w, h);
                composite(&[(&src, 1.0)], &mut o, fit).unwrap();
                assert_eq!(o.data, src.data, "{w}x{h} {fit:?}");
            }
        }
    }

    #[test]
    fn exact_copy_also_through_the_blend_path() {
        // Two layers, one with weight 0, take the general accumulator path.
        let src = noise(97, 31);
        let other = noise(13, 50);
        let mut o = out(97, 31);
        composite(&[(&src, 1.0), (&other, 0.0)], &mut o, Fit::Fill).unwrap();
        assert_eq!(o.data, src.data);
    }

    #[test]
    fn half_blend_of_two_solids_is_the_midpoint() {
        let a = VideoFrame::solid(Micros(0), 320, 180, [200, 0, 10, 255]);
        let b = VideoFrame::solid(Micros(0), 640, 360, [0, 100, 255, 255]);
        let mut o = out(320, 180);
        composite(&[(&a, 0.5), (&b, 0.5)], &mut o, Fit::Fill).unwrap();
        let want = [100.0, 50.0, 132.5, 255.0];
        for p in o.data.chunks_exact(4) {
            for c in 0..4 {
                assert!((f32::from(p[c]) - want[c]).abs() <= 1.0, "{p:?}");
            }
        }
    }

    /// 320x240 source: red rows 0..40, blue rows 200..240, green between.
    fn banded() -> VideoFrame {
        let mut f = VideoFrame::solid(Micros(0), 320, 240, GREEN);
        for y in 0..240usize {
            let c = match y {
                0..40 => RED,
                200..240 => BLUE,
                _ => continue,
            };
            for x in 0..320usize {
                f.data[(y * 320 + x) * 4..][..4].copy_from_slice(&c);
            }
        }
        f
    }

    #[test]
    fn fill_4_3_into_16_9_crops_top_and_bottom_equally() {
        let src = banded();
        let mut o = out(1920, 1080);
        composite(&[(&src, 1.0)], &mut o, Fit::Fill).unwrap();
        // Scaled to 1920x1440, 180 output rows (30 source rows) cut per side:
        // 10 red source rows -> 60 output rows remain on top, 60 blue at the
        // bottom.
        let red_rows = (0..1080).filter(|&y| px(&o, 960, y)[0] > 127).count();
        let blue_rows = (0..1080).filter(|&y| px(&o, 960, y)[2] > 127).count();
        assert_eq!(red_rows, blue_rows);
        assert!((59..=61).contains(&red_rows), "{red_rows}");
        assert!((0..red_rows as u32).all(|y| px(&o, 960, y)[0] > 127));
        assert!((1080 - blue_rows as u32..1080).all(|y| px(&o, 960, y)[2] > 127));
        assert_eq!(px(&o, 0, 540), GREEN);
        assert_eq!(px(&o, 1919, 540), GREEN);
        assert_eq!(px(&o, 960, 10), RED);
        assert_eq!(px(&o, 960, 1069), BLUE);
        // Full width is used: no bars.
        assert!((0..1080).all(|y| px(&o, 0, y) != BLACK && px(&o, 1919, y) != BLACK));
    }

    #[test]
    fn fit_adds_equal_black_bars() {
        // Letterbox: 16:9 into 4:3, no scaling -> exact bars of 30 rows.
        let src = VideoFrame::solid(Micros(0), 320, 180, [250, 240, 230, 255]);
        let mut o = out(320, 240);
        composite(&[(&src, 1.0)], &mut o, Fit::Fit).unwrap();
        for y in 0..240 {
            let want = if (30..210).contains(&y) {
                [250, 240, 230, 255]
            } else {
                BLACK
            };
            assert!((0..320).all(|x| px(&o, x, y) == want), "row {y}");
        }
        // Pillarbox with upscaling: 4:3 into 16:9 -> 1440 wide, 240 px bars.
        let src = VideoFrame::solid(Micros(0), 320, 240, RED);
        let mut o = out(1920, 1080);
        composite(&[(&src, 1.0)], &mut o, Fit::Fit).unwrap();
        for x in 0..1920 {
            let want = if (240..1680).contains(&x) { RED } else { BLACK };
            assert_eq!(px(&o, x, 0), want, "col {x}");
            assert_eq!(px(&o, x, 1079), want, "col {x}");
        }
    }

    #[test]
    fn fit_bars_blend_as_black() {
        // A letterboxed layer at 50% over a full-frame layer: bars show the
        // full layer at half brightness.
        let wide = VideoFrame::solid(Micros(0), 320, 180, [200, 200, 200, 255]);
        let full = VideoFrame::solid(Micros(0), 320, 240, [100, 100, 100, 255]);
        let mut o = out(320, 240);
        composite(&[(&wide, 0.5), (&full, 0.5)], &mut o, Fit::Fit).unwrap();
        assert_eq!(px(&o, 0, 0), [50, 50, 50, 255]);
        assert_eq!(px(&o, 0, 120), [150, 150, 150, 255]);
    }

    /// Stand-in for `kv_testkit::render_tagged_frame` (#24, not merged yet):
    /// a 4x3 grid of 80 px blocks, each a distinct colour.
    fn grid() -> (VideoFrame, Vec<[u8; 4]>) {
        let colours: Vec<[u8; 4]> = (0..12u8)
            .map(|i| [i * 20, 255 - i * 20, ((u32::from(i) * 67) % 251) as u8, 255])
            .collect();
        let mut f = VideoFrame::solid(Micros(0), 320, 240, BLACK);
        for y in 0..240usize {
            for x in 0..320usize {
                let c = colours[(y / 80) * 4 + x / 80];
                f.data[(y * 320 + x) * 4..][..4].copy_from_slice(&c);
            }
        }
        (f, colours)
    }

    #[test]
    fn coded_blocks_read_back_after_fill() {
        let (src, colours) = grid();
        // (out size, scale, rows cropped per side)
        for (w, h, scale, crop) in [(1920u32, 1080u32, 6u32, 180u32), (640, 360, 2, 60)] {
            let mut o = out(w, h);
            composite(&[(&src, 1.0)], &mut o, Fit::Fill).unwrap();
            for by in 0..3u32 {
                for bx in 0..4u32 {
                    let x = (40 + 80 * bx) * scale;
                    let y = (40 + 80 * by) * scale - crop;
                    assert_eq!(
                        px(&o, x, y),
                        colours[(by * 4 + bx) as usize],
                        "{w}x{h} block {bx},{by}"
                    );
                }
            }
        }
    }

    #[test]
    fn rejects_malformed_frames() {
        let bad = VideoFrame {
            data: vec![0; 5],
            ..VideoFrame::solid(Micros(0), 2, 2, BLACK)
        };
        let good = VideoFrame::solid(Micros(0), 2, 2, BLACK);
        let mut o = good.clone();
        assert!(matches!(
            composite(&[(&bad, 1.0)], &mut o, Fit::Fill),
            Err(CompositeError::InvalidFrame { which: "layer", .. })
        ));
        let mut bad_out = bad.clone();
        assert!(matches!(
            composite(&[(&good, 1.0)], &mut bad_out, Fit::Fill),
            Err(CompositeError::InvalidFrame {
                which: "output",
                ..
            })
        ));
    }

    #[test]
    fn empty_sizes_are_harmless() {
        let empty = VideoFrame::solid(Micros(0), 0, 0, BLACK);
        let mut o = out(8, 8);
        composite(&[(&empty, 1.0)], &mut o, Fit::Fill).unwrap();
        assert!(o.data.chunks_exact(4).all(|p| p == BLACK));
        let mut e = empty.clone();
        composite(&[(&o, 1.0)], &mut e, Fit::Fit).unwrap();
        assert!(e.data.is_empty());
    }

    /// Logs the 1080p two-layer crossfade time (target under 15 ms in
    /// release on the CI runner; not asserted). Run with
    /// `cargo test --release -p kv-render timing -- --nocapture`.
    #[test]
    fn timing_1080p_two_layer_crossfade() {
        // Typical export: decoder pre-scaled to the output size.
        let a = noise(1920, 1080);
        let b = noise(1920, 1080);
        // Worst case: 4:3 source upscaled and cropped.
        let c = noise(1440, 1080);
        let mut o = out(1920, 1080);
        for (name, second) in [("pre-scaled", &b), ("4:3 fill", &c)] {
            composite(&[(&a, 0.5), (second, 0.5)], &mut o, Fit::Fill).unwrap();
            let runs = 5u32;
            let start = std::time::Instant::now();
            for _ in 0..runs {
                composite(&[(&a, 0.5), (second, 0.5)], &mut o, Fit::Fill).unwrap();
            }
            let ms = start.elapsed().as_secs_f64() * 1000.0 / f64::from(runs);
            println!("composite 1080p two-layer ({name}): {ms:.2} ms/frame");
        }
    }
}
