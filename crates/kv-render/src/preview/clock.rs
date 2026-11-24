//! Playback clocks: what "now" is while the preview plays.

use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Instant;

use kv_core::Micros;

/// A monotonic time source that drives playback. The engine reads it to pick
/// the frame to show; frames are never chosen by how fast they decode.
///
/// [`SystemClock`] is the default; the audio output later provides a clock
/// driven by the samples actually played, so picture follows sound.
pub trait Clock: Send + Sync {
    /// Current time. Must never go backwards; the origin is arbitrary.
    fn now(&self) -> Micros;
}

/// Monotonic wall-clock time since the clock was created.
#[derive(Debug, Clone, Copy)]
pub struct SystemClock {
    start: Instant,
}

impl SystemClock {
    pub fn new() -> Self {
        SystemClock {
            start: Instant::now(),
        }
    }
}

impl Default for SystemClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for SystemClock {
    fn now(&self) -> Micros {
        Micros(i64::try_from(self.start.elapsed().as_micros()).unwrap_or(i64::MAX))
    }
}

/// A clock that only moves when told to, for deterministic tests.
#[derive(Debug, Default)]
pub struct ManualClock {
    now: AtomicI64,
}

impl ManualClock {
    /// A clock at time 0.
    pub fn new() -> Self {
        Self::default()
    }

    /// Jump to `t`. Callers keep it monotonic.
    pub fn set(&self, t: Micros) {
        self.now.store(t.0, Ordering::SeqCst);
    }

    /// Move forward by `d`.
    pub fn advance(&self, d: Micros) {
        self.now.fetch_add(d.0, Ordering::SeqCst);
    }
}

impl Clock for ManualClock {
    fn now(&self) -> Micros {
        Micros(self.now.load(Ordering::SeqCst))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_clock_is_monotonic() {
        let c = SystemClock::new();
        let a = c.now();
        let b = c.now();
        assert!(a.0 >= 0 && b >= a);
    }

    #[test]
    fn manual_clock_moves_only_when_told() {
        let c = ManualClock::new();
        assert_eq!(c.now(), Micros(0));
        c.advance(Micros(40));
        c.advance(Micros(2));
        assert_eq!(c.now(), Micros(42));
        c.set(Micros(1_000));
        assert_eq!(c.now(), Micros(1_000));
    }
}
