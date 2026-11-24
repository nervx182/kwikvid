//! One stats implementation for every path: the webview sends raw samples,
//! the native path collects them in Rust, both end up as a `PathResult`.

use serde::{Deserialize, Serialize};

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Samples {
    /// Frame-to-screen latency per displayed frame, ms.
    #[serde(default)]
    pub latency_ms: Vec<f64>,
    /// Interval between consecutive displayed frames, ms.
    #[serde(default)]
    pub interval_ms: Vec<f64>,
    /// Rust-side frame production cost (generate, plus encode for JPEG), ms.
    #[serde(default)]
    pub gen_ms: Vec<f64>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PathResult {
    pub path: String,
    pub seconds: f64,
    /// Distinct frames that reached the screen.
    pub displayed: u64,
    /// Frames produced/received (may exceed displayed when superseded).
    pub produced: u64,
    pub fps: f64,
    pub produced_fps: f64,
    pub latency_p50_ms: f64,
    pub latency_p95_ms: f64,
    pub latency_max_ms: f64,
    pub interval_p50_ms: f64,
    pub interval_p95_ms: f64,
    /// Display intervals longer than 1.5x the 60 Hz budget (16.7 ms).
    pub long_frames: usize,
    pub gen_p50_ms: f64,
    pub notes: String,
}

impl PathResult {
    pub fn from_samples(
        path: &str,
        seconds: f64,
        displayed: u64,
        produced: u64,
        s: &Samples,
        notes: String,
    ) -> Self {
        let secs = seconds.max(1e-9);
        Self {
            path: path.to_string(),
            seconds,
            displayed,
            produced,
            fps: displayed as f64 / secs,
            produced_fps: produced as f64 / secs,
            latency_p50_ms: pct(&s.latency_ms, 0.50),
            latency_p95_ms: pct(&s.latency_ms, 0.95),
            latency_max_ms: pct(&s.latency_ms, 1.0),
            interval_p50_ms: pct(&s.interval_ms, 0.50),
            interval_p95_ms: pct(&s.interval_ms, 0.95),
            long_frames: s.interval_ms.iter().filter(|&&i| i > 25.0).count(),
            gen_p50_ms: pct(&s.gen_ms, 0.50),
            notes,
        }
    }

    pub fn summary_line(&self) -> String {
        format!(
            "{:<12} fps {:>6.1} (produced {:>6.1})  latency p50 {:>6.1} ms p95 {:>6.1} ms max {:>6.1} ms  interval p50 {:>5.1} p95 {:>5.1} ms  long {:>4}  gen p50 {:>5.1} ms",
            self.path,
            self.fps,
            self.produced_fps,
            self.latency_p50_ms,
            self.latency_p95_ms,
            self.latency_max_ms,
            self.interval_p50_ms,
            self.interval_p95_ms,
            self.long_frames,
            self.gen_p50_ms
        )
    }
}

/// Nearest-rank percentile; NaN for no samples (serialises as null).
fn pct(v: &[f64], p: f64) -> f64 {
    if v.is_empty() {
        return f64::NAN;
    }
    let mut s: Vec<f64> = v.iter().copied().filter(|x| x.is_finite()).collect();
    if s.is_empty() {
        return f64::NAN;
    }
    s.sort_by(|a, b| a.total_cmp(b));
    let idx = ((p * s.len() as f64).ceil() as usize).clamp(1, s.len()) - 1;
    s[idx]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentiles_and_fps() {
        let s = Samples {
            latency_ms: (1..=100).map(f64::from).collect(),
            interval_ms: vec![16.7, 16.7, 33.4],
            gen_ms: vec![2.0],
        };
        let r = PathResult::from_samples("x", 2.0, 120, 150, &s, String::new());
        assert_eq!(r.fps, 60.0);
        assert_eq!(r.produced_fps, 75.0);
        assert_eq!(r.latency_p50_ms, 50.0);
        assert_eq!(r.latency_p95_ms, 95.0);
        assert_eq!(r.latency_max_ms, 100.0);
        assert_eq!(r.long_frames, 1);
        assert!(pct(&[], 0.5).is_nan());
    }
}
