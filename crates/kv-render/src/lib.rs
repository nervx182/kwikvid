//! kwikvid rendering.
//!
//! - The frame plan: the pure mapping from an edit decision list ([`Edl`])
//!   to output frames, each listing the source frames visible in it and
//!   their blend weights ([`plan`], [`FramePlan`]).
//! - The CPU export compositor (ADR 0009): turns a frame's decoded layers
//!   into output pixels ([`composite`], [`Fit`], [`source_size_for`]).
//! - The export pipeline: a project section to an MP4 ([`export()`]).

mod composite;
mod export;

pub use composite::{composite, source_size_for, CompositeError, Fit};
pub use export::{export, ExportReport, Progress, RenderError, MAX_OPEN_READERS, MUSIC_FADE_OUT};

use kv_core::{Edl, MediaId, Micros, Segment, Speed, Transition};
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
    /// Time in the source media to show, within `[src_in, src_out)`.
    pub src_time: Micros,
    /// Blend weight in `(0, 1]`; the layers of a frame sum to 1.
    pub weight: f32,
}

/// One output frame and the source layers composited into it.
#[derive(Debug, Clone, PartialEq)]
pub struct OutputFrame {
    pub index: u64,
    /// Presentation time on the section timeline.
    pub pts: Micros,
    /// At most two layers, outgoing first. Empty means a gap: draw black.
    pub layers: Vec<Layer>,
}

/// The output frames of an EDL at a frame rate. Frames are computed on
/// demand, so a plan is cheap to build for any edit length.
#[derive(Debug, Clone)]
pub struct FramePlan<'a> {
    edl: &'a Edl,
    fps: Rational,
    len: u64,
}

/// Plan the output frames of `edl` at `fps`.
///
/// Frame `k` has `pts = round(k * 1e6 * den / num)` microseconds, computed
/// exactly in integers. The plan holds every frame whose pts is before the
/// end of the edit (the latest `timeline_start + timeline_len`).
pub fn plan(edl: &Edl, fps: Rational) -> Result<FramePlan<'_>, PlanError> {
    if fps.num <= 0 || fps.den <= 0 {
        return Err(PlanError::InvalidFps {
            num: fps.num,
            den: fps.den,
        });
    }
    let end = edl.segments.iter().map(segment_end).max().unwrap_or(0);
    let len = frames_before(end, fps);
    Ok(FramePlan { edl, fps, len })
}

impl<'a> FramePlan<'a> {
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
            layers: layers_at(self.edl, pts),
        })
    }

    /// All frames in order.
    pub fn iter(&self) -> impl Iterator<Item = OutputFrame> + '_ {
        (0..self.len).filter_map(move |k| self.frame(k))
    }
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

/// The layers visible at `pts`.
///
/// When several segments cover `pts` the last two (by EDL order) are
/// blended: the later one is incoming. With a crossfade its weight rises
/// linearly from 0 at its `timeline_start` to 1 after the fade length (the
/// crossfade duration, shortened to the actual overlap if `repack` clamped
/// it). A cut, or an overlap without a crossfade, shows the incoming segment
/// alone. Layers with zero weight are dropped.
fn layers_at(edl: &Edl, pts: i64) -> Vec<Layer> {
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
    let layer = |(i, s): (usize, &Segment), weight: f32| Layer {
        media_id: s.media_id,
        segment: i,
        src_time: src_time(s, pts),
        weight,
    };
    match (out, inc) {
        (None, _) => Vec::new(),
        (Some(only), None) => vec![layer(only, 1.0)],
        (Some(o), Some(n)) => {
            let w = incoming_weight(o.1, n.1, pts);
            if w >= 1.0 {
                vec![layer(n, 1.0)]
            } else if w <= 0.0 {
                vec![layer(o, 1.0)]
            } else {
                vec![layer(o, 1.0 - w), layer(n, w)]
            }
        }
    }
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

    fn frames(e: &Edl, fps: Rational) -> Vec<OutputFrame> {
        plan(e, fps).unwrap().iter().collect()
    }

    #[test]
    fn empty_edl_has_no_frames() {
        let e = Edl::default();
        let p = plan(&e, FPS30).unwrap();
        assert!(p.is_empty());
        assert_eq!(p.iter().count(), 0);
        assert_eq!(p.frame(0), None);
    }

    #[test]
    fn invalid_fps_is_rejected() {
        let e = Edl::default();
        assert_eq!(
            plan(&e, Rational::new(0, 1)).unwrap_err(),
            PlanError::InvalidFps { num: 0, den: 1 }
        );
        assert!(plan(&e, Rational::new(30, 0)).is_err());
        assert!(plan(&e, Rational::new(-30, 1)).is_err());
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
        let p = plan(&e, Rational::new(10, 1)).unwrap();
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
        let p = plan(&e, Rational::new(10, 1)).unwrap();
        let f5 = p.frame(5).unwrap();
        assert!((f5.layers[1].weight - 0.5).abs() < 1e-6);
        assert_eq!(p.frame(10).unwrap().layers.len(), 1);
    }

    #[test]
    fn speed_50_is_slow_motion() {
        // 1 s of source at 50 % lasts 2 s.
        let e = edl(vec![seg(1, 1_000_000, 2_000_000, Transition::Cut, 50)]);
        let p = plan(&e, Rational::new(10, 1)).unwrap();
        assert_eq!(p.len(), 20);
        for f in p.iter() {
            assert_eq!(f.layers[0].src_time, Micros(1_000_000 + f.pts.0 / 2));
        }
    }

    #[test]
    fn speed_200_is_double_speed() {
        // 2 s of source at 200 % lasts 1 s.
        let e = edl(vec![seg(1, 0, 2_000_000, Transition::Cut, 200)]);
        let p = plan(&e, Rational::new(10, 1)).unwrap();
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
        let last = layers_at(&e, segment_end(&e.segments[0]) - 1);
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
        let p = plan(&e, FPS30).unwrap();
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
        let p = plan(&e, NTSC).unwrap();
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
        assert_eq!(plan(&e, Rational::new(10, 1)).unwrap().len(), 1);
        let e = edl(vec![seg(1, 0, 100_001, Transition::Cut, 100)]);
        assert_eq!(plan(&e, Rational::new(10, 1)).unwrap().len(), 2);
    }
}
