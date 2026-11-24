//! Tempo estimation by autocorrelation of the onset envelope.

/// Slowest tempo considered, in BPM.
pub(crate) const MIN_BPM: f32 = 40.0;
/// Fastest tempo considered, in BPM.
pub(crate) const MAX_BPM: f32 = 240.0;
/// Tempi in this range are equally likely a priori; outside it the prior
/// decays as a log-normal with `PRIOR_OCTAVES` standard deviation. Within the
/// plateau, ties between a tempo and its half go to the faster one because the
/// biased autocorrelation shrinks with lag.
const PLATEAU_BPM: (f32, f32) = (55.0, 190.0);
const PRIOR_OCTAVES: f32 = 0.4;

/// Half-width of the window used for local tempo, in seconds.
const LOCAL_HALF_WINDOW_SECS: f32 = 3.0;
/// Spacing of local tempo estimates, in seconds.
const LOCAL_STEP_SECS: f32 = 0.5;

fn prior(bpm: f32) -> f32 {
    let (lo, hi) = PLATEAU_BPM;
    let edge = if bpm < lo {
        lo
    } else if bpm > hi {
        hi
    } else {
        return 1.0;
    };
    let z = (bpm / edge).log2() / PRIOR_OCTAVES;
    (-0.5 * z * z).exp()
}

/// Lag range in frames for the tempo range, given the envelope frame rate.
pub(crate) fn lag_range(frame_rate: f32) -> (usize, usize) {
    let min_lag = (60.0 * frame_rate / MAX_BPM).floor().max(1.0) as usize;
    let max_lag = (60.0 * frame_rate / MIN_BPM).ceil() as usize;
    (min_lag, max_lag)
}

/// Biased autocorrelation of the mean-removed signal for lags `0..=max_lag`.
fn autocorr(x: &[f32], max_lag: usize) -> Vec<f32> {
    let n = x.len();
    let mean = x.iter().sum::<f32>() / n.max(1) as f32;
    let d: Vec<f32> = x.iter().map(|&v| v - mean).collect();
    (0..=max_lag)
        .map(|lag| {
            if lag >= n {
                return 0.0;
            }
            d[..n - lag]
                .iter()
                .zip(&d[lag..])
                .map(|(a, b)| a * b)
                .sum::<f32>()
                / n as f32
        })
        .collect()
}

/// Best beat period, in (fractional) frames, of an onset envelope, or `None`
/// if it has no periodic structure.
pub(crate) fn estimate_period(env: &[f32], frame_rate: f32) -> Option<f32> {
    let (min_lag, max_lag) = lag_range(frame_rate);
    if env.len() <= min_lag + 2 {
        return None;
    }
    let max_lag = max_lag.min(env.len() - 2);
    let ac = autocorr(&smooth3(env), max_lag + 1);
    let is_peak = |lag: usize| ac[lag] > 0.0 && ac[lag] >= ac[lag - 1] && ac[lag] >= ac[lag + 1];
    let score = |lag: usize| ac[lag] * prior(60.0 * frame_rate / lag as f32);
    let mut best: Option<(usize, f32)> = None;
    for lag in (min_lag..=max_lag).filter(|&l| is_peak(l)) {
        let s = score(lag);
        if best.is_none_or(|(_, b)| s > b) {
            best = Some((lag, s));
        }
    }
    let (mut lag, mut best_score) = best?;
    // A pulse train with period L also repeats at 2L, and rounding to whole
    // frames can make the 2L peak slightly higher. Prefer the faster tempo
    // when its peak is nearly as strong.
    loop {
        let half = lag / 2;
        let faster = (half.saturating_sub(1)..=half + 1)
            .filter(|&l| l >= min_lag && is_peak(l))
            .map(|l| (l, score(l)))
            .max_by(|a, b| a.1.total_cmp(&b.1));
        match faster {
            Some((l, s)) if s >= 0.9 * best_score => {
                lag = l;
                best_score = s;
            }
            _ => break,
        }
    }
    // Parabolic interpolation around the peak for sub-frame precision.
    let (y0, y1, y2) = (ac[lag - 1], ac[lag], ac[lag + 1]);
    let denom = y0 - 2.0 * y1 + y2;
    let offset = if denom.abs() > f32::EPSILON {
        (0.5 * (y0 - y2) / denom).clamp(-0.5, 0.5)
    } else {
        0.0
    };
    Some(lag as f32 + offset)
}

/// Light [1, 2, 1] / 4 smoothing so a beat whose onset straddles two frames
/// still gives one autocorrelation peak.
fn smooth3(x: &[f32]) -> Vec<f32> {
    (0..x.len())
        .map(|i| {
            let prev = if i > 0 { x[i - 1] } else { 0.0 };
            let next = x.get(i + 1).copied().unwrap_or(0.0);
            0.25 * prev + 0.5 * x[i] + 0.25 * next
        })
        .collect()
}

/// Per-frame local beat period, in frames, estimated over sliding windows and
/// kept in the same octave as `global`. Windows without periodic structure use
/// `global`.
pub(crate) fn local_periods(env: &[f32], frame_rate: f32, global: f32) -> Vec<f32> {
    let n = env.len();
    let half = (LOCAL_HALF_WINDOW_SECS * frame_rate).round() as usize;
    let step = ((LOCAL_STEP_SECS * frame_rate).round() as usize).max(1);
    if n <= 2 * half {
        return vec![global; n];
    }

    let centres: Vec<usize> = (0..n).step_by(step).collect();
    let mut est: Vec<f32> = centres
        .iter()
        .map(|&c| {
            let lo = c.saturating_sub(half);
            let hi = (c + half).min(n);
            // Keep windows full length at the edges.
            let (lo, hi) = if hi - lo < 2 * half {
                if lo == 0 {
                    (0, 2 * half)
                } else {
                    (n - 2 * half, n)
                }
            } else {
                (lo, hi)
            };
            estimate_period(&env[lo..hi], frame_rate)
                .map(|p| same_octave(p, global))
                .unwrap_or(global)
        })
        .collect();
    median_filter(&mut est, 2);

    (0..n)
        .map(|i| est[((i + step / 2) / step).min(est.len() - 1)])
        .collect()
}

/// Map `p` to `p * 2^k` when that is close to `reference` (within 8%), to
/// avoid octave jumps between local estimates. Other ratios (real tempo
/// changes) are kept.
fn same_octave(p: f32, reference: f32) -> f32 {
    [0.5f32, 1.0, 2.0]
        .into_iter()
        .map(|k| p * k)
        .find(|&q| (q / reference - 1.0).abs() < 0.08)
        .unwrap_or(p)
}

fn median_filter(x: &mut [f32], radius: usize) {
    let src = x.to_vec();
    for (i, out) in x.iter_mut().enumerate() {
        let lo = i.saturating_sub(radius);
        let hi = (i + radius + 1).min(src.len());
        let mut w = src[lo..hi].to_vec();
        w.sort_by(f32::total_cmp);
        *out = w[w.len() / 2];
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FR: f32 = 172.265_63; // 22050 / 128

    fn pulses(period: f32, n: usize) -> Vec<f32> {
        let mut x = vec![0.0; n];
        let mut t = 0.0f32;
        while (t.round() as usize) < n {
            x[t.round() as usize] = 1.0;
            t += period;
        }
        x
    }

    #[test]
    fn prior_is_flat_on_plateau_and_decays_outside() {
        assert_eq!(prior(60.0), 1.0);
        assert_eq!(prior(180.0), 1.0);
        assert!(prior(240.0) < 0.9);
        assert!(prior(30.0) < 0.3);
    }

    #[test]
    fn pulse_train_period_is_found() {
        for bpm in [60.0f32, 100.0, 128.0, 174.0] {
            let period = 60.0 * FR / bpm;
            let p = estimate_period(&pulses(period, 3000), FR).unwrap();
            assert!((p - period).abs() < 1.0, "{bpm} BPM: {p} vs {period}");
        }
    }

    #[test]
    fn flat_envelope_has_no_period() {
        assert_eq!(estimate_period(&vec![1.0; 2000], FR), None);
        assert_eq!(estimate_period(&[], FR), None);
    }

    #[test]
    fn same_octave_snaps_only_octaves() {
        assert_eq!(same_octave(100.0, 50.0), 50.0);
        assert_eq!(same_octave(25.0, 50.0), 50.0);
        assert_eq!(same_octave(70.0, 50.0), 70.0);
    }
}
