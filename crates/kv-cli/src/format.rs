//! Output helpers shared by the subcommands.

use std::io::{self, Write};

use kv_core::Micros;
use serde::Serialize;

/// Seconds with millisecond precision, e.g. `12.500`.
pub fn secs(t: Micros) -> String {
    let sign = if t.0 < 0 { "-" } else { "" };
    let ms = (t.0.unsigned_abs() + 500) / 1_000;
    format!("{sign}{}.{:03}", ms / 1_000, ms % 1_000)
}

/// Tempo rounded to 0.01 BPM, so JSON output does not depend on the last
/// bits of floating-point arithmetic.
pub fn round_bpm(bpm: f64) -> f64 {
    (bpm * 100.0).round() / 100.0
}

/// Pretty-print `value` as JSON on stdout, followed by a newline.
pub fn print_json(value: &impl Serialize) -> anyhow::Result<()> {
    let mut out = io::stdout().lock();
    serde_json::to_writer_pretty(&mut out, value)?;
    writeln!(out)?;
    Ok(())
}

/// Write `text` to stdout.
pub fn print_text(text: &str) -> anyhow::Result<()> {
    io::stdout().lock().write_all(text.as_bytes())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seconds_format() {
        assert_eq!(secs(Micros(0)), "0.000");
        assert_eq!(secs(Micros(12_500_000)), "12.500");
        assert_eq!(secs(Micros(1_999_600)), "2.000");
        assert_eq!(secs(Micros(-1_500)), "-0.002");
    }

    #[test]
    fn bpm_rounding() {
        assert_eq!(round_bpm(119.99999), 120.0);
        assert_eq!(round_bpm(127.456), 127.46);
    }
}
