//! Onset strength envelope by spectral flux, and onset peak picking.

use rustfft::num_complex::Complex;
use rustfft::FftPlanner;

/// STFT frame length in samples at the analysis rate.
pub(crate) const N_FFT: usize = 1024;
/// STFT hop in samples at the analysis rate.
pub(crate) const HOP: usize = 128;
/// Log compression gain: `ln(1 + GAMMA * magnitude)`.
const GAMMA: f32 = 100.0;

/// Spectral-flux onset strength, one value per hop. Frame `i` is centred on
/// sample `i * HOP` (the signal is zero-padded by `N_FFT / 2` on both sides),
/// so its time is `i * HOP / sample_rate`.
pub(crate) fn onset_envelope(samples: &[f32]) -> Vec<f32> {
    if samples.is_empty() {
        return Vec::new();
    }
    let n_frames = 1 + samples.len() / HOP;
    let half = N_FFT / 2;
    let window: Vec<f32> = (0..N_FFT)
        .map(|i| {
            let x = std::f32::consts::TAU * i as f32 / N_FFT as f32;
            0.5 - 0.5 * x.cos()
        })
        .collect();
    // Normalise so a full-scale sine has magnitude about 1.
    let norm = 2.0 / window.iter().sum::<f32>();
    let n_bins = half + 1;

    let fft = FftPlanner::<f32>::new().plan_fft_forward(N_FFT);
    let mut buf = vec![Complex::new(0.0f32, 0.0); N_FFT];
    let mut scratch = vec![Complex::new(0.0f32, 0.0); fft.get_inplace_scratch_len()];
    let mut prev = vec![0.0f32; n_bins];
    let mut cur = vec![0.0f32; n_bins];
    let mut env = Vec::with_capacity(n_frames);

    for frame in 0..n_frames {
        let centre = frame * HOP;
        for (j, slot) in buf.iter_mut().enumerate() {
            // Sample index in the unpadded signal.
            let idx = (centre + j).wrapping_sub(half);
            let s = samples.get(idx).copied().unwrap_or(0.0);
            *slot = Complex::new(s * window[j], 0.0);
        }
        fft.process_with_scratch(&mut buf, &mut scratch);
        let mut flux = 0.0f32;
        for (k, c) in cur.iter_mut().enumerate() {
            *c = (1.0 + GAMMA * norm * buf[k].norm()).ln();
            // Before the first frame is silence, so sound at t = 0 is an
            // onset too.
            flux += (*c - prev[k]).max(0.0);
        }
        env.push(flux);
        std::mem::swap(&mut prev, &mut cur);
    }
    env
}

/// Divide by the standard deviation so thresholds are scale-free. A silent
/// (all-zero) envelope is returned unchanged.
pub(crate) fn normalise(env: &[f32]) -> Vec<f32> {
    let n = env.len().max(1) as f32;
    let mean = env.iter().sum::<f32>() / n;
    let var = env.iter().map(|&x| (x - mean) * (x - mean)).sum::<f32>() / n;
    let std = var.sqrt();
    if std <= f32::EPSILON {
        return env.to_vec();
    }
    env.iter().map(|&x| x / std).collect()
}

/// Pick onset frames from a normalised envelope: local maxima within
/// `±max_radius` frames that exceed the local mean over `±mean_radius` frames
/// by `delta`, at least `min_gap` frames apart.
pub(crate) fn pick_onsets(
    env: &[f32],
    max_radius: usize,
    mean_radius: usize,
    delta: f32,
    min_gap: usize,
) -> Vec<usize> {
    let mut out: Vec<usize> = Vec::new();
    for i in 0..env.len() {
        let lo = i.saturating_sub(max_radius);
        let hi = (i + max_radius + 1).min(env.len());
        if env[lo..hi].iter().any(|&v| v > env[i]) {
            continue;
        }
        let lo = i.saturating_sub(mean_radius);
        let hi = (i + mean_radius + 1).min(env.len());
        let mean = env[lo..hi].iter().sum::<f32>() / (hi - lo) as f32;
        if env[i] < mean + delta {
            continue;
        }
        if out.last().is_some_and(|&last| i - last < min_gap) {
            continue;
        }
        out.push(i);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn silence_has_no_flux() {
        let env = onset_envelope(&vec![0.0; 22_050]);
        assert_eq!(env.len(), 1 + 22_050 / HOP);
        assert!(env.iter().all(|&v| v == 0.0));
        assert!(pick_onsets(&normalise(&env), 5, 20, 0.5, 5).is_empty());
    }

    #[test]
    fn impulse_peaks_near_its_time() {
        let mut x = vec![0.0f32; 22_050];
        for v in &mut x[11_025..11_050] {
            *v = 0.8;
        }
        let env = normalise(&onset_envelope(&x));
        let onsets = pick_onsets(&env, 5, 20, 0.5, 5);
        assert_eq!(onsets.len(), 1, "{onsets:?}");
        let t = onsets[0] as f64 * HOP as f64 / 22_050.0;
        assert!((t - 0.5).abs() < 0.03, "onset at {t}");
    }
}
