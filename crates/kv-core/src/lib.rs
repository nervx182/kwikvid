//! kwikvid core: project model, edit decision list (EDL) and the `Command` API.
//!
//! This crate is a placeholder created with the repository skeleton. The real
//! types arrive with the "kv-core: EDL and Command types" issue. Keep this crate
//! free of I/O and platform code (see `docs/architecture.md`).

use serde::{Deserialize, Serialize};

/// Time on a timeline or within a media file, in microseconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Micros(pub i64);

impl Micros {
    /// Construct from seconds (rounded to the nearest microsecond).
    pub fn from_secs_f64(secs: f64) -> Self {
        Micros((secs * 1_000_000.0).round() as i64)
    }

    /// Value in seconds.
    pub fn as_secs_f64(self) -> f64 {
        self.0 as f64 / 1_000_000.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn micros_round_trip() {
        let t = Micros::from_secs_f64(1.5);
        assert_eq!(t, Micros(1_500_000));
        assert_eq!(t.as_secs_f64(), 1.5);
    }

    #[test]
    fn micros_serde() {
        let json = serde_json::to_string(&Micros(42)).unwrap();
        assert_eq!(json, "42");
    }
}
