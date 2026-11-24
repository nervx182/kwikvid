//! Beat tracking by dynamic programming (after Ellis 2007, "Beat Tracking by
//! Dynamic Programming"), extended with a per-frame target period so that
//! tempo changes are followed.

/// Weight of the tempo-consistency penalty relative to onset strength.
const TIGHTNESS: f32 = 100.0;

/// Track beats in a normalised onset envelope. `periods[i]` is the expected
/// beat period, in frames, at frame `i` (same length as `env`). Returns beat
/// frames in increasing order.
pub(crate) fn track(env: &[f32], periods: &[f32]) -> Vec<usize> {
    let n = env.len();
    if n == 0 || env.iter().all(|&v| v <= 0.0) {
        return Vec::new();
    }
    debug_assert_eq!(periods.len(), n);
    let local = smooth(env, periods);

    // cumulative[i]: best score of a beat sequence ending with a beat at i.
    let mut cumulative = vec![0.0f32; n];
    let mut back: Vec<Option<usize>> = vec![None; n];
    for i in 0..n {
        let tau = periods[i];
        let lo = i as f32 - 2.0 * tau;
        let hi = i as f32 - 0.5 * tau;
        let mut best: Option<(usize, f32)> = None;
        if hi >= 0.0 {
            let lo = lo.max(0.0).round() as usize;
            let hi = (hi.round() as usize).min(i.saturating_sub(1));
            for (p, &prev) in cumulative.iter().enumerate().take(hi + 1).skip(lo) {
                let r = ((i - p) as f32 / tau).ln();
                let score = prev - TIGHTNESS * r * r;
                if best.is_none_or(|(_, s)| score > s) {
                    best = Some((p, score));
                }
            }
        }
        match best {
            // Starting a new sequence here is allowed when no predecessor
            // helps (e.g. after a long silence).
            Some((p, s)) if s > 0.0 => {
                cumulative[i] = local[i] + s;
                back[i] = Some(p);
            }
            _ => cumulative[i] = local[i],
        }
    }

    let Some(last) = last_beat(&cumulative) else {
        return Vec::new();
    };
    let mut beats = vec![last];
    let mut cur = last;
    while let Some(p) = back[cur] {
        beats.push(p);
        cur = p;
    }
    beats.reverse();
    let mut beats = trim_weak(&beats, &local);
    snap_to_peaks(&mut beats, &local, periods);
    beats
}

/// Move each beat to the strongest onset within a 16th of the period: the
/// dynamic programme tolerates one-frame slips because its tempo penalty is
/// nearly flat near the target period.
fn snap_to_peaks(beats: &mut [usize], local: &[f32], periods: &[f32]) {
    for b in beats.iter_mut() {
        let r = ((periods[*b] / 16.0).round() as usize).max(1);
        let lo = b.saturating_sub(r);
        let hi = (*b + r + 1).min(local.len());
        if let Some(best) = (lo..hi).max_by(|&x, &y| local[x].total_cmp(&local[y])) {
            *b = best;
        }
    }
}

/// Smooth the envelope with a short Gaussian (width about a 32nd of the beat
/// period) so slightly early or late onsets still score.
fn smooth(env: &[f32], periods: &[f32]) -> Vec<f32> {
    let n = env.len();
    let mean_period = periods.iter().sum::<f32>() / n as f32;
    let sigma = (mean_period / 32.0).max(0.5);
    let radius = (3.0 * sigma).ceil() as isize;
    let kernel: Vec<f32> = (-radius..=radius)
        .map(|k| (-0.5 * (k as f32 / sigma).powi(2)).exp())
        .collect();
    (0..n as isize)
        .map(|i| {
            kernel
                .iter()
                .zip(-radius..=radius)
                .filter_map(|(w, k)| env.get(usize::try_from(i + k).ok()?).map(|v| v * w))
                .sum()
        })
        .collect()
}

/// The last local maximum of the cumulative score that reaches half the
/// median of all local maxima.
fn last_beat(cumulative: &[f32]) -> Option<usize> {
    let n = cumulative.len();
    let is_peak = |i: usize| {
        let c = cumulative[i];
        (i == 0 || c > cumulative[i - 1]) && (i + 1 == n || c >= cumulative[i + 1])
    };
    let mut peaks: Vec<f32> = (0..n)
        .filter(|&i| is_peak(i))
        .map(|i| cumulative[i])
        .collect();
    if peaks.is_empty() {
        return None;
    }
    peaks.sort_by(f32::total_cmp);
    let threshold = 0.5 * peaks[peaks.len() / 2];
    (0..n)
        .rev()
        .find(|&i| is_peak(i) && cumulative[i] >= threshold)
}

/// Drop beats at the start and end whose onset strength is below half the
/// RMS strength of all beats (silence or fade-in/out).
fn trim_weak(beats: &[usize], local: &[f32]) -> Vec<usize> {
    if beats.is_empty() {
        return Vec::new();
    }
    let rms = (beats.iter().map(|&b| local[b] * local[b]).sum::<f32>() / beats.len() as f32).sqrt();
    let threshold = 0.5 * rms;
    let start = beats.iter().position(|&b| local[b] >= threshold);
    let end = beats.iter().rposition(|&b| local[b] >= threshold);
    match (start, end) {
        (Some(s), Some(e)) => beats[s..=e].to_vec(),
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tracks_regular_pulses() {
        let mut env = vec![0.0f32; 2000];
        let truth: Vec<usize> = (0..20).map(|k| 50 + k * 95).collect();
        for &t in &truth {
            env[t] = 10.0;
        }
        let beats = track(&env, &vec![95.0; env.len()]);
        assert_eq!(beats, truth);
    }

    #[test]
    fn silent_envelope_has_no_beats() {
        assert!(track(&vec![0.0; 500], &vec![80.0; 500]).is_empty());
        assert!(track(&[], &[]).is_empty());
    }
}
