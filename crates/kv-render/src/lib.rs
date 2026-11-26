//! kwikvid rendering.
//!
//! - The frame plan: the pure mapping from an edit decision list ([`Edl`])
//!   to output frames, each listing the source frames visible in it and
//!   their blend weights ([`plan`], [`FramePlan`]). Slow motion blends the
//!   two source frames around the exact source time (ADR 0005). Overlays
//!   (the title card, [`FramePlan::with_title`]) sit on top with an opacity.
//! - The title card: text rasterised to a transparent RGBA layer
//!   ([`title::rasterize`]).
//! - The CPU export compositor (ADR 0009): turns a frame's decoded layers
//!   into output pixels ([`composite`], [`Fit`], [`source_size_for`]), then
//!   alpha-blends overlays on top ([`overlay()`]).
//! - The export pipeline: a project section to an MP4 ([`export()`]).
//! - The GPU preview compositor (ADR 0007), the same compositing on wgpu
//!   ([`gpu::GpuCompositor`], feature `gpu`, on by default).
//! - The live preview engine: playback clock, decode-ahead, seek
//!   ([`preview::PreviewEngine`]).

mod composite;
mod export;
#[cfg(feature = "gpu")]
pub mod gpu;
pub mod preview;
pub mod title;

pub use composite::{composite, overlay, source_size_for, CompositeError, Fit};
pub use export::{
    export, export_with_lookahead, ExportReport, Progress, RenderError, DECODE_AHEAD,
    MAX_OPEN_READERS, MUSIC_FADE_OUT,
};

use kv_core::{Edl, MediaId, Micros, Segment, Speed, TitleCard, Transition};
use kv_media::Rational;

/// Errors from building a frame plan.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PlanError {
    /// The frame rate must have a positive numerator and denominator.
    #[error("invalid frame rate {num}/{den}: numerator and denominator must be positive")]
    InvalidFps { num: i32, den: i32 },
}

/// One source frame visible in an output frame.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Layer {
    pub media_id: MediaId,
    /// Index of the segment in [`Edl::segments`].
    pub segment: usize,
    /// Time in the source media to show, within `[src_in, src_out)`. When
    /// the source frame rate is known this is the pts of the source frame
    /// to show (raised to `src_in` if that frame starts before it), so
    /// `VideoReader::frame_at(src_time)` returns exactly that frame.
    pub src_time: Micros,
    /// Blend weight in `(0, 1]`; the layers of a frame sum to 1.
    pub weight: f32,
}

/// Which overlay image an [`Overlay`] shows. The renderer owns the images
/// (rasterised once per export or preview), the plan only says when and how
/// strongly each one is visible.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OverlayId {
    /// The section's title card ([`title::rasterize`]).
    Title,
}

/// An image alpha-blended over an output frame after its source layers
/// ([`overlay()`]).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Overlay {
    pub id: OverlayId,
    /// Multiplies the image's own alpha, in `[0, 1]`.
    pub opacity: f32,
}

/// Fade-in and fade-out length of the title card (guess: 300 ms each). A
/// title shorter than two fades fades over half its length each way.
pub const TITLE_FADE: Micros = Micros(300_000);

/// One output frame and the source layers composited into it.
#[derive(Debug, Clone, PartialEq)]
pub struct OutputFrame {
    pub index: u64,
    /// Presentation time on the section timeline.
    pub pts: Micros,
    /// At most four layers, outgoing segment first: one or two segments
    /// (two in a crossfade), each shown as one source frame, or as the two
    /// source frames around the exact source time in slow motion
    /// (`speed < 100`), earlier frame first. Empty means a gap: draw black.
    pub layers: Vec<Layer>,
    /// Drawn over the composited layers, in order.
    pub overlays: Vec<Overlay>,
}

/// The frame rate of each source media item, for [`plan`]: `None` (or an
/// invalid rate) when unknown, which disables snapping and blending for
/// that media.
pub type SourceFps<'a> = &'a dyn Fn(MediaId) -> Option<Rational>;

/// The output frames of an EDL at a frame rate. Frames are computed on
/// demand, so a plan is cheap to build for any edit length.
#[derive(Clone)]
pub struct FramePlan<'a> {
    edl: &'a Edl,
    fps: Rational,
    source_fps: SourceFps<'a>,
    len: u64,
    /// Title card duration, if the section has one.
    title: Option<Micros>,
}

impl std::fmt::Debug for FramePlan<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FramePlan")
            .field("edl", self.edl)
            .field("fps", &self.fps)
            .field("len", &self.len)
            .field("title", &self.title)
            .finish_non_exhaustive()
    }
}

/// Plan the output frames of `edl` at `fps`; `source_fps` gives each
/// media item's frame rate (`VideoInfo::fps`).
///
/// Frame `k` has `pts = round(k * 1e6 * den / num)` microseconds, computed
/// exactly in integers. The plan holds every frame whose pts is before the
/// end of the edit (the latest `timeline_start + timeline_len`).
///
/// With a known source frame rate, source frame `j` is taken to start at
/// `round(j * 1e6 * den / num)` (stream-relative, like reader times) and
/// layers are snapped to it. A segment at `speed < 100` whose exact source
/// time falls between frames `j` and `j + 1`, at fraction `f`, shows both:
/// `(j, 1 - f)` and `(j + 1, f)`. Exactly on a frame (within 1 us of
/// rounding) it shows that frame alone; frame `j + 1` is never shown when
/// it starts at or after `src_out`.
pub fn plan<'a>(
    edl: &'a Edl,
    fps: Rational,
    source_fps: SourceFps<'a>,
) -> Result<FramePlan<'a>, PlanError> {
    if fps.num <= 0 || fps.den <= 0 {
        return Err(PlanError::InvalidFps {
            num: fps.num,
            den: fps.den,
        });
    }
    let end = edl.segments.iter().map(segment_end).max().unwrap_or(0);
    let len = frames_before(end, fps);
    Ok(FramePlan {
        edl,
        fps,
        source_fps,
        len,
        title: None,
    })
}

impl<'a> FramePlan<'a> {
    /// Show `title` (a section's [`TitleCard`]) over the opening frames: an
    /// [`OverlayId::Title`] overlay on every frame with pts in
    /// `[0, title.duration)`, its opacity rising linearly from 0 at pts 0 to
    /// 1 after [`TITLE_FADE`] and falling back towards 0 over the last
    /// [`TITLE_FADE`] (see [`title_opacity`]). `None` shows no title. The
    /// plan's length is unchanged: a title longer than the edit is cut short.
    pub fn with_title(mut self, title: Option<&TitleCard>) -> Self {
        self.title = title.map(|t| t.duration);
        self
    }

    /// Number of output frames.
    pub fn len(&self) -> u64 {
        self.len
    }

    /// True when the plan has no frames (empty EDL).
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The frame rate this plan was built for.
    pub fn fps(&self) -> Rational {
        self.fps
    }

    /// Frame `index`, or `None` past the end.
    pub fn frame(&self, index: u64) -> Option<OutputFrame> {
        if index >= self.len {
            return None;
        }
        let pts = frame_pts(index, self.fps);
        Some(OutputFrame {
            index,
            pts: Micros(pts),
            layers: layers_at(self.edl, pts, self.source_fps),
            overlays: self
                .title
                .and_then(|d| title_opacity(d, Micros(pts)))
                .map(|opacity| Overlay {
                    id: OverlayId::Title,
                    opacity,
                })
                .into_iter()
                .collect(),
        })
    }

    /// All frames in order.
    pub fn iter(&self) -> impl Iterator<Item = OutputFrame> + '_ {
        (0..self.len).filter_map(move |k| self.frame(k))
    }
}

/// Opacity of a title card lasting `duration` at time `pts`, or `None`
/// outside `[0, duration)`: `min(1, pts / fade, (duration - pts) / fade)`
/// with `fade = min(TITLE_FADE, duration / 2)`. So it is 0 at pts 0, 1 on
/// the plateau, and reaches 0 exactly at `duration` (the last frame shown
/// has a small positive opacity).
pub fn title_opacity(duration: Micros, pts: Micros) -> Option<f32> {
    if pts.0 < 0 || pts.0 >= duration.0 {
        return None;
    }
    let fade = TITLE_FADE.0.min(duration.0 / 2).max(1) as f64;
    let rise = pts.0 as f64 / fade;
    let fall = (duration.0 - pts.0) as f64 / fade;
    Some(rise.min(fall).min(1.0) as f32)
}

/// `round(k * 1e6 * den / num)`, exact, for positive `num` and `den`.
fn frame_pts(k: u64, fps: Rational) -> i64 {
    let n = i128::from(k) * 1_000_000 * i128::from(fps.den);
    let d = i128::from(fps.num);
    let rounded = (2 * n + d) / (2 * d);
    i64::try_from(rounded).unwrap_or(i64::MAX)
}

/// Number of frames whose pts is strictly before `end`.
fn frames_before(end: i64, fps: Rational) -> u64 {
    if end <= 0 {
        return 0;
    }
    // Estimate, then correct for rounding: pts is non-decreasing in k.
    let est = i128::from(end) * i128::from(fps.num) / (1_000_000 * i128::from(fps.den));
    let mut k = u64::try_from(est).unwrap_or(u64::MAX);
    while k > 0 && frame_pts(k - 1, fps) >= end {
        k -= 1;
    }
    while frame_pts(k, fps) < end {
        k += 1;
    }
    k
}

fn segment_end(seg: &Segment) -> i64 {
    seg.timeline_start.0.saturating_add(seg.timeline_len().0)
}

fn covers(seg: &Segment, pts: i64) -> bool {
    seg.timeline_start.0 <= pts && pts < segment_end(seg)
}

/// Source time shown by `seg` at timeline time `pts`, clamped to
/// `[src_in, src_out)`.
fn src_time(seg: &Segment, pts: i64) -> Micros {
    let offset = i128::from(pts) - i128::from(seg.timeline_start.0);
    let pct = i128::from(seg.speed.0.max(1));
    let t = i128::from(seg.src_in.0) + offset * pct / i128::from(Speed::NORMAL.0);
    let last = i128::from(seg.src_out.0) - 1;
    let lo = i128::from(seg.src_in.0);
    let t = t.min(last).max(lo);
    Micros(i64::try_from(t).unwrap_or(i64::MAX))
}

/// The source frames shown for `seg` at its exact source time `t`, as
/// `(src_time, weight)` with weights summing to 1 (see [`plan`]).
fn source_frames(seg: &Segment, t: Micros, fps: Option<Rational>) -> Vec<(Micros, f64)> {
    let Some(fps) = fps.filter(|r| r.num > 0 && r.den > 0) else {
        return vec![(t, 1.0)];
    };
    let t = t.0;
    if t < 0 {
        return vec![(Micros(t), 1.0)];
    }
    // `j`: the last source frame starting at or before `t`.
    let est = i128::from(t) * i128::from(fps.num) / (1_000_000 * i128::from(fps.den));
    let mut j = u64::try_from(est).unwrap_or(u64::MAX - 1);
    while j > 0 && frame_pts(j, fps) > t {
        j -= 1;
    }
    while frame_pts(j + 1, fps) <= t {
        j += 1;
    }
    let (a, b) = (frame_pts(j, fps), frame_pts(j + 1, fps));
    let snap = |p: i64| Micros(p.max(seg.src_in.0));
    // Within 1 us of a frame start: that frame (absorbs pts rounding).
    let next_in_range = b < seg.src_out.0;
    if next_in_range && b - t <= 1 {
        return vec![(snap(b), 1.0)];
    }
    if t - a <= 1 || !next_in_range || seg.speed.0 >= Speed::NORMAL.0 {
        return vec![(snap(a), 1.0)];
    }
    let f = (t - a) as f64 / (b - a) as f64;
    vec![(snap(a), 1.0 - f), (snap(b), f)]
}

/// The layers visible at `pts`.
///
/// When several segments cover `pts` the last two (by EDL order) are
/// blended: the later one is incoming. With a crossfade its weight rises
/// linearly from 0 at its `timeline_start` to 1 after the fade length (the
/// crossfade duration, shortened to the actual overlap if `repack` clamped
/// it). A cut, or an overlap without a crossfade, shows the incoming segment
/// alone. Layers with zero weight are dropped. Each segment then expands to
/// its [`source_frames`], so a slow-motion crossfade has up to four layers.
fn layers_at(edl: &Edl, pts: i64, source_fps: SourceFps<'_>) -> Vec<Layer> {
    // The last two covering segments: `out` then `inc` (if any).
    let mut out = None;
    let mut inc = None;
    for c in edl
        .segments
        .iter()
        .enumerate()
        .filter(|(_, s)| covers(s, pts))
    {
        if let Some(prev) = inc.replace(c) {
            out = Some(prev);
        }
    }
    let (out, inc) = match out {
        Some(o) => (Some(o), inc),
        None => (inc, None),
    };
    let shown: Vec<((usize, &Segment), f32)> = match (out, inc) {
        (None, _) => Vec::new(),
        (Some(only), None) => vec![(only, 1.0)],
        (Some(o), Some(n)) => {
            let w = incoming_weight(o.1, n.1, pts);
            if w >= 1.0 {
                vec![(n, 1.0)]
            } else if w <= 0.0 {
                vec![(o, 1.0)]
            } else {
                vec![(o, 1.0 - w), (n, w)]
            }
        }
    };
    let mut layers = Vec::with_capacity(4);
    for ((i, s), weight) in shown {
        let frames = source_frames(s, src_time(s, pts), source_fps(s.media_id));
        for (t, f) in frames {
            let w = (f64::from(weight) * f) as f32;
            if w > 0.0 {
                layers.push(Layer {
                    media_id: s.media_id,
                    segment: i,
                    src_time: t,
                    weight: w,
                });
            }
        }
    }
    layers
}

fn incoming_weight(out: &Segment, inc: &Segment, pts: i64) -> f32 {
    let Transition::Crossfade { duration } = inc.transition else {
        return 1.0;
    };
    let overlap = segment_end(out) - inc.timeline_start.0;
    let fade = duration.0.min(overlap);
    if fade <= 0 {
        return 1.0;
    }
    let elapsed = pts - inc.timeline_start.0;
    (elapsed as f64 / fade as f64).clamp(0.0, 1.0) as f32
}

#[cfg(test)]
mod tests {
    use super::*;

    const FPS30: Rational = Rational::new(30, 1);
    const NTSC: Rational = Rational::new(30000, 1001);

    fn seg(media: u64, src_in: i64, src_out: i64, transition: Transition, speed: u32) -> Segment {
        Segment {
            media_id: MediaId(media),
            src_in: Micros(src_in),
            src_out: Micros(src_out),
            timeline_start: Micros(0),
            transition,
            speed: Speed(speed),
        }
    }

    fn edl(segments: Vec<Segment>) -> Edl {
        let mut e = Edl { segments };
        e.repack();
        e
    }

    /// Source frame rate unknown: no snapping, no blending.
    fn no_fps(_: MediaId) -> Option<Rational> {
        None
    }

    /// Every source at 30 fps.
    fn src30(_: MediaId) -> Option<Rational> {
        Some(FPS30)
    }

    fn frames(e: &Edl, fps: Rational) -> Vec<OutputFrame> {
        plan(e, fps, &no_fps).unwrap().iter().collect()
    }

    fn summary(layers: &[Layer]) -> Vec<(u64, i64, f32)> {
        layers
            .iter()
            .map(|l| (l.media_id.0, l.src_time.0, l.weight))
            .collect()
    }

    fn assert_weights(f: &OutputFrame, want: &[(u64, i64, f32)]) {
        let got = summary(&f.layers);
        assert_eq!(got.len(), want.len(), "frame {}: {got:?}", f.index);
        for (g, w) in got.iter().zip(want) {
            assert_eq!((g.0, g.1), (w.0, w.1), "frame {}: {got:?}", f.index);
            assert!((g.2 - w.2).abs() < 1e-4, "frame {}: {got:?}", f.index);
        }
        let sum: f32 = f.layers.iter().map(|l| l.weight).sum();
        assert!((sum - 1.0).abs() < 1e-6, "frame {}: sum {sum}", f.index);
    }

    #[test]
    fn title_overlay_only_in_range_with_fades() {
        // 3 s edit at 10 fps; a 2 s title: frames 0..20.
        let e = edl(vec![seg(1, 0, 3_000_000, Transition::Cut, 100)]);
        let card = TitleCard {
            text: "Hi".into(),
            duration: Micros(2_000_000),
        };
        let p = plan(&e, Rational::new(10, 1), &no_fps)
            .unwrap()
            .with_title(Some(&card));
        assert_eq!(p.len(), 30);
        for f in p.iter() {
            let want = match f.index {
                // Fade in over 300 ms: 0, 1/3, 2/3.
                k @ 0..=2 => Some(k as f32 / 3.0),
                3..=17 => Some(1.0),
                // Fade out: (2 s - pts) / 300 ms = 2/3, 1/3.
                k @ 18..=19 => Some((20 - k) as f32 / 3.0),
                _ => None,
            };
            match (want, f.overlays.as_slice()) {
                (None, []) => {}
                (Some(w), [o]) => {
                    assert_eq!(o.id, OverlayId::Title);
                    assert!((o.opacity - w).abs() < 1e-5, "frame {}: {o:?}", f.index);
                }
                (w, got) => panic!("frame {}: want {w:?}, got {got:?}", f.index),
            }
            // Layers are unaffected by the title.
            assert_eq!(f.layers.len(), 1);
        }
        // No title, or `None`: no overlays anywhere.
        let plain = plan(&e, Rational::new(10, 1), &no_fps).unwrap();
        assert!(plain.iter().all(|f| f.overlays.is_empty()));
        let none = plain.clone().with_title(None);
        assert!(none.iter().all(|f| f.overlays.is_empty()));
    }

    #[test]
    fn title_opacity_shapes() {
        let d = Micros(2_000_000);
        assert_eq!(title_opacity(d, Micros(-1)), None);
        assert_eq!(title_opacity(d, Micros(0)), Some(0.0));
        assert_eq!(title_opacity(d, Micros(150_000)), Some(0.5));
        assert_eq!(title_opacity(d, Micros(300_000)), Some(1.0));
        assert_eq!(title_opacity(d, Micros(1_700_000)), Some(1.0));
        assert_eq!(title_opacity(d, Micros(1_850_000)), Some(0.5));
        assert_eq!(title_opacity(d, d), None);
        // Shorter than two fades: a triangle peaking at the middle.
        let short = Micros(500_000);
        assert_eq!(title_opacity(short, Micros(250_000)), Some(1.0));
        assert_eq!(title_opacity(short, Micros(125_000)), Some(0.5));
        assert_eq!(title_opacity(short, Micros(375_000)), Some(0.5));
    }

    #[test]
    fn empty_edl_has_no_frames() {
        let e = Edl::default();
        let p = plan(&e, FPS30, &no_fps).unwrap();
        assert!(p.is_empty());
        assert_eq!(p.iter().count(), 0);
        assert_eq!(p.frame(0), None);
    }

    #[test]
    fn invalid_fps_is_rejected() {
        let e = Edl::default();
        assert_eq!(
            plan(&e, Rational::new(0, 1), &no_fps).unwrap_err(),
            PlanError::InvalidFps { num: 0, den: 1 }
        );
        assert!(plan(&e, Rational::new(30, 0), &no_fps).is_err());
        assert!(plan(&e, Rational::new(-30, 1), &no_fps).is_err());
    }

    #[test]
    fn single_segment_maps_source_time() {
        // One second at 30 fps: 30 frames, pts 0, 33333, 66667, ...
        let e = edl(vec![seg(7, 5_000_000, 6_000_000, Transition::Cut, 100)]);
        let f = frames(&e, FPS30);
        assert_eq!(f.len(), 30);
        assert_eq!(f[1].pts, Micros(33_333));
        assert_eq!(f[2].pts, Micros(66_667));
        assert_eq!(f[29].pts, Micros(966_667));
        for fr in &f {
            assert_eq!(fr.layers.len(), 1);
            let l = fr.layers[0];
            assert_eq!(l.media_id, MediaId(7));
            assert_eq!(l.segment, 0);
            assert_eq!(l.weight, 1.0);
            assert_eq!(l.src_time, Micros(5_000_000 + fr.pts.0));
        }
    }

    #[test]
    fn cut_frame_at_boundary_shows_new_segment_only() {
        // 1 s then 1 s: frame 30 is exactly at 1 s.
        let e = edl(vec![
            seg(1, 0, 1_000_000, Transition::Cut, 100),
            seg(2, 3_000_000, 4_000_000, Transition::Cut, 100),
        ]);
        let f = frames(&e, FPS30);
        assert_eq!(f.len(), 60);
        assert_eq!(f[29].layers[0].media_id, MediaId(1));
        assert_eq!(f[30].pts, Micros(1_000_000));
        assert_eq!(
            f[30].layers,
            vec![Layer {
                media_id: MediaId(2),
                segment: 1,
                src_time: Micros(3_000_000),
                weight: 1.0,
            }]
        );
    }

    #[test]
    fn crossfade_weights_rise_linearly() {
        // 2 s, then 2 s with a 1 s crossfade: overlap is [1 s, 2 s).
        let e = edl(vec![
            seg(1, 0, 2_000_000, Transition::Cut, 100),
            seg(
                2,
                10_000_000,
                12_000_000,
                Transition::Crossfade {
                    duration: Micros(1_000_000),
                },
                100,
            ),
        ]);
        assert_eq!(e.segments[1].timeline_start, Micros(1_000_000));
        let p = plan(&e, Rational::new(10, 1), &no_fps).unwrap();
        assert_eq!(p.len(), 30); // ends at 3 s

        // Before the overlap: outgoing alone.
        assert_eq!(p.frame(9).unwrap().layers.len(), 1);
        // At the overlap start the incoming weight is 0: outgoing alone.
        let f10 = p.frame(10).unwrap();
        assert_eq!(f10.layers.len(), 1);
        assert_eq!(f10.layers[0].media_id, MediaId(1));
        // Inside: linear.
        for k in 11..20u64 {
            let f = p.frame(k).unwrap();
            assert_eq!(f.layers.len(), 2);
            let (o, n) = (f.layers[0], f.layers[1]);
            assert_eq!((o.media_id, n.media_id), (MediaId(1), MediaId(2)));
            let expect = (k - 10) as f32 / 10.0;
            assert!((n.weight - expect).abs() < 1e-6, "frame {k}: {}", n.weight);
            assert!((o.weight + n.weight - 1.0).abs() < 1e-6);
            assert_eq!(o.src_time, Micros(k as i64 * 100_000));
            assert_eq!(n.src_time, Micros(10_000_000 + (k as i64 - 10) * 100_000));
        }
        // After the overlap: incoming alone.
        let f20 = p.frame(20).unwrap();
        assert_eq!(f20.layers.len(), 1);
        assert_eq!(f20.layers[0].media_id, MediaId(2));
        assert_eq!(f20.layers[0].weight, 1.0);
    }

    #[test]
    fn crossfade_clamped_by_repack_still_reaches_full_weight() {
        // Crossfade of 2 s into a 1 s previous segment: overlap is 1 s.
        let e = edl(vec![
            seg(1, 0, 1_000_000, Transition::Cut, 100),
            seg(
                2,
                0,
                2_000_000,
                Transition::Crossfade {
                    duration: Micros(2_000_000),
                },
                100,
            ),
        ]);
        assert_eq!(e.segments[1].timeline_start, Micros(0));
        let p = plan(&e, Rational::new(10, 1), &no_fps).unwrap();
        let f5 = p.frame(5).unwrap();
        assert!((f5.layers[1].weight - 0.5).abs() < 1e-6);
        assert_eq!(p.frame(10).unwrap().layers.len(), 1);
    }

    #[test]
    fn speed_50_is_slow_motion() {
        // 1 s of source at 50 % lasts 2 s.
        let e = edl(vec![seg(1, 1_000_000, 2_000_000, Transition::Cut, 50)]);
        let p = plan(&e, Rational::new(10, 1), &no_fps).unwrap();
        assert_eq!(p.len(), 20);
        for f in p.iter() {
            assert_eq!(f.layers[0].src_time, Micros(1_000_000 + f.pts.0 / 2));
        }
    }

    #[test]
    fn speed_200_is_double_speed() {
        // 2 s of source at 200 % lasts 1 s.
        let e = edl(vec![seg(1, 0, 2_000_000, Transition::Cut, 200)]);
        let p = plan(&e, Rational::new(10, 1), &no_fps).unwrap();
        assert_eq!(p.len(), 10);
        for f in p.iter() {
            assert_eq!(f.layers[0].src_time, Micros(f.pts.0 * 2));
        }
    }

    #[test]
    fn source_time_is_clamped_to_source_range() {
        let s = seg(1, 1_000, 2_000, Transition::Cut, 300);
        assert_eq!(src_time(&s, -5), Micros(1_000));
        assert_eq!(src_time(&s, 0), Micros(1_000));
        assert_eq!(src_time(&s, 10_000), Micros(1_999));
        // Speed 300 with integer timeline length: the last covered instant
        // maps just below src_out.
        let e = edl(vec![s]);
        let last = layers_at(&e, segment_end(&e.segments[0]) - 1, &no_fps);
        assert!(last[0].src_time < Micros(2_000));
    }

    #[test]
    fn gap_yields_empty_layers() {
        let mut e = edl(vec![
            seg(1, 0, 1_000_000, Transition::Cut, 100),
            seg(2, 0, 1_000_000, Transition::Cut, 100),
        ]);
        // Not repacked: leave [1 s, 2 s) uncovered.
        e.segments[1].timeline_start = Micros(2_000_000);
        let p = plan(&e, FPS30, &no_fps).unwrap();
        assert_eq!(p.len(), 90);
        assert!(p.frame(29).unwrap().layers.len() == 1);
        assert!(p.frame(30).unwrap().layers.is_empty());
        assert!(p.frame(59).unwrap().layers.is_empty());
        assert_eq!(p.frame(60).unwrap().layers[0].media_id, MediaId(2));
    }

    #[test]
    fn ntsc_timing_is_exact_over_ten_minutes() {
        // 10 minutes at 30000/1001.
        let e = edl(vec![seg(1, 0, 600_000_000, Transition::Cut, 100)]);
        let p = plan(&e, NTSC, &no_fps).unwrap();
        // 600 s * 30000/1001 = 17982.01...: frames 0..=17982.
        assert_eq!(p.len(), 17_983);
        assert_eq!(p.frame(1).unwrap().pts, Micros(33_367)); // 33366.67
        assert_eq!(p.frame(30_000).map(|f| f.pts), None);
        // Every 30000 frames is exactly 1001 s; check exactness at frame
        // multiples of 30 (1.001 s each) and no drift at the end.
        for k in (0..p.len()).step_by(30) {
            assert_eq!(p.frame(k).unwrap().pts, Micros(k as i64 / 30 * 1_001_000));
        }
        let last = p.frame(p.len() - 1).unwrap();
        assert_eq!(last.pts, Micros(599_999_400)); // 17982 * 1001/30000 s
        assert!(last.pts < Micros(600_000_000));
        assert!(frame_pts(p.len(), NTSC) >= 600_000_000);
    }

    #[test]
    fn frame_count_boundary_is_exclusive() {
        // An edit ending exactly on a frame boundary excludes that frame.
        let e = edl(vec![seg(1, 0, 100_000, Transition::Cut, 100)]);
        assert_eq!(plan(&e, Rational::new(10, 1), &no_fps).unwrap().len(), 1);
        let e = edl(vec![seg(1, 0, 100_001, Transition::Cut, 100)]);
        assert_eq!(plan(&e, Rational::new(10, 1), &no_fps).unwrap().len(), 2);
    }

    #[test]
    fn slow_motion_blends_the_frames_around_the_source_time() {
        // 1 s of a 30 fps source from 1 s, at 50 %, output at 30 fps.
        let e = edl(vec![seg(1, 1_000_000, 2_000_000, Transition::Cut, 50)]);
        let p = plan(&e, FPS30, &src30).unwrap();
        assert_eq!(p.len(), 60);
        for f in p.iter() {
            let j = 30 + f.index as i64 / 2; // source frame at or before
            let pj = frame_pts(j as u64, FPS30);
            if f.index % 2 == 0 {
                // Exactly on source frame j: one layer, snapped to its pts.
                assert_weights(&f, &[(1, pj, 1.0)]);
            } else if j == 59 {
                // Frame 60 is at src_out: hold frame 59.
                assert_weights(&f, &[(1, pj, 1.0)]);
            } else {
                let pn = frame_pts(j as u64 + 1, FPS30);
                assert_weights(&f, &[(1, pj, 0.5), (1, pn, 0.5)]);
            }
        }
    }

    #[test]
    fn slow_motion_weights_follow_the_fractional_position() {
        // 25 % at 30 fps output on a 30 fps source: quarters.
        let e = edl(vec![seg(1, 0, 1_000_000, Transition::Cut, 25)]);
        let p = plan(&e, FPS30, &src30).unwrap();
        let (p1, p2) = (frame_pts(1, FPS30), frame_pts(2, FPS30));
        assert_weights(&p.frame(4).unwrap(), &[(1, p1, 1.0)]);
        assert_weights(&p.frame(5).unwrap(), &[(1, p1, 0.75), (1, p2, 0.25)]);
        assert_weights(&p.frame(6).unwrap(), &[(1, p1, 0.5), (1, p2, 0.5)]);
        assert_weights(&p.frame(7).unwrap(), &[(1, p1, 0.25), (1, p2, 0.75)]);
        assert_weights(&p.frame(8).unwrap(), &[(1, p2, 1.0)]);
    }

    #[test]
    fn layers_snap_to_source_frame_pts() {
        // Normal speed, 24 fps output of a 30 fps source starting off-grid:
        // one layer at the pts of the frame shown, never blended.
        let src_in = 1_010_000; // inside source frame 30 (1 s .. 1.0333 s)
        let e = edl(vec![seg(1, src_in, 2_000_000, Transition::Cut, 100)]);
        let p = plan(&e, Rational::new(24, 1), &src30).unwrap();
        for f in p.iter() {
            assert_eq!(f.layers.len(), 1);
            let l = f.layers[0];
            let t = src_in + f.pts.0;
            let j = t * 30 / 1_000_000;
            let want = frame_pts(j as u64, FPS30).max(src_in);
            assert_eq!(l.src_time, Micros(want), "frame {}", f.index);
            assert!(l.src_time.0 <= t);
        }
        // The first frame starts before src_in: raised to src_in.
        assert_eq!(p.frame(0).unwrap().layers[0].src_time, Micros(src_in));

        // A source time 1 us short of a frame (pts rounding) is that frame.
        let s = seg(1, 0, 1_000_000, Transition::Cut, 100);
        assert_eq!(
            source_frames(&s, Micros(66_666), Some(FPS30)),
            vec![(Micros(66_667), 1.0)]
        );
        // Unknown or invalid rate: the time as is.
        assert_eq!(
            source_frames(&s, Micros(66_000), None),
            vec![(Micros(66_000), 1.0)]
        );
        assert_eq!(
            source_frames(&s, Micros(66_000), Some(Rational::new(0, 1))),
            vec![(Micros(66_000), 1.0)]
        );
        // NTSC source: frame 1 starts at 33367.
        assert_eq!(
            source_frames(&s, Micros(40_000), Some(NTSC)),
            vec![(Micros(33_367), 1.0)]
        );
    }

    #[test]
    fn slow_motion_crossfade_has_four_layers() {
        // Two 1 s sources at 50 % (2 s each) with a 1 s crossfade, output
        // and sources at 30 fps: the overlap is [1 s, 2 s).
        let e = edl(vec![
            seg(1, 0, 1_000_000, Transition::Cut, 50),
            seg(
                2,
                3_000_000,
                4_000_000,
                Transition::Crossfade {
                    duration: Micros(1_000_000),
                },
                50,
            ),
        ]);
        assert_eq!(e.segments[1].timeline_start, Micros(1_000_000));
        let p = plan(&e, FPS30, &src30).unwrap();
        assert_eq!(p.len(), 90);

        // Frame 45 (1.5 s): incoming weight 0.5; outgoing source at 0.75 s
        // (between frames 22 and 23, f = 0.5), incoming at 3.25 s (between
        // 97 and 98, f = 0.5).
        let f = p.frame(45).unwrap();
        assert_weights(
            &f,
            &[
                (1, frame_pts(22, FPS30), 0.25),
                (1, frame_pts(23, FPS30), 0.25),
                (2, frame_pts(97, FPS30), 0.25),
                (2, frame_pts(98, FPS30), 0.25),
            ],
        );
        assert_eq!(
            f.layers.iter().map(|l| l.segment).collect::<Vec<_>>(),
            [0, 0, 1, 1]
        );
        // Frame 46: both on a source frame again: two layers.
        let f = p.frame(46).unwrap();
        assert_eq!(f.layers.len(), 2);
        // Every frame of the overlap has 2 or 4 layers, never more...
        for k in 31..59 {
            let n = p.frame(k).unwrap().layers.len();
            assert!(n == 2 || n == 4, "frame {k}: {n} layers");
        }
        // ...except the last, where the outgoing source holds its last
        // frame (the next one starts at src_out) and the incoming blends.
        let f = p.frame(59).unwrap();
        assert_eq!(
            f.layers.iter().map(|l| l.segment).collect::<Vec<_>>(),
            [0, 1, 1]
        );
        assert_eq!(f.layers[0].src_time, Micros(frame_pts(29, FPS30)));
    }
}
