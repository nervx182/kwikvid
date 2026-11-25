//! Parsers for the small value syntaxes on the command line: times, target
//! lengths, `CLIP@START-END` ranges, `CLIP=SECONDS` durations, output sizes
//! and frame rates. Pure
//! string handling; clip names are resolved against the clip list later.

use kv_core::Micros;
use kv_media::Rational;

/// `--length`: a fixed length or the whole song.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Length {
    Fixed(Micros),
    FullSong,
}

/// A `--pin` or `--exclude` value: a clip name and an optional range
/// (`None` means the whole clip; only allowed for excludes).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RangeSpec {
    /// The value exactly as typed, for error messages.
    pub text: String,
    pub clip: String,
    pub range: Option<(Micros, Micros)>,
}

/// A `--durations` entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClipDuration {
    pub clip: String,
    pub duration: Micros,
}

/// Parse a time: seconds (`12.5`, `12.5s`), minutes and seconds (`1m30s`,
/// `2m`) or `M:SS(.f)` (`1:30.5`). Must be finite and non-negative.
pub fn parse_time(s: &str) -> Result<Micros, String> {
    let bad = || format!("invalid time {s:?}: expected seconds like 12.5, 1m30s or 1:30");
    let t = s.trim();
    let secs = if let Some((m, sec)) = t.split_once(':') {
        minutes(m).zip(seconds(sec)).map(|(m, s)| m + s)
    } else if let Some((m, rest)) = t.split_once('m') {
        let rest = rest.strip_suffix('s').unwrap_or(rest);
        let sec = if rest.is_empty() {
            Some(0.0)
        } else {
            seconds(rest)
        };
        minutes(m).zip(sec).map(|(m, s)| m + s)
    } else {
        seconds(t.strip_suffix('s').unwrap_or(t))
    };
    match secs {
        Some(v) if (0.0..1e9).contains(&v) => Ok(Micros::from_secs_f64(v)),
        _ => Err(bad()),
    }
}

/// Whole minutes, in seconds.
fn minutes(s: &str) -> Option<f64> {
    if s.is_empty() || !s.bytes().all(|c| c.is_ascii_digit()) {
        return None;
    }
    s.parse::<f64>().ok().map(|m| m * 60.0)
}

/// Decimal seconds without sign or exponent.
fn seconds(s: &str) -> Option<f64> {
    let ok = !s.is_empty()
        && s.bytes().all(|c| c.is_ascii_digit() || c == b'.')
        && s.bytes().any(|c| c.is_ascii_digit());
    if ok {
        s.parse().ok()
    } else {
        None
    }
}

/// Parse `--length`: `full` or a positive time.
pub fn parse_length(s: &str) -> Result<Length, String> {
    if s.trim().eq_ignore_ascii_case("full") {
        return Ok(Length::FullSong);
    }
    let t = parse_time(s)?;
    if t.0 <= 0 {
        return Err(format!("length must be positive, got {s:?}"));
    }
    Ok(Length::Fixed(t))
}

/// Parse a `--pin` value: `CLIP@START-END`.
pub fn parse_pin(s: &str) -> Result<RangeSpec, String> {
    let spec = parse_range_spec(s)?;
    if spec.range.is_none() {
        return Err(format!(
            "pin {s:?} needs a range: CLIP@START-END, e.g. clip.mp4@12.5-15.0"
        ));
    }
    Ok(spec)
}

/// Parse an `--exclude` value: `CLIP` or `CLIP@START-END`.
pub fn parse_exclude(s: &str) -> Result<RangeSpec, String> {
    parse_range_spec(s)
}

fn parse_range_spec(s: &str) -> Result<RangeSpec, String> {
    let (clip, range) = match s.rsplit_once('@') {
        Some((clip, range)) => {
            let (a, b) = range
                .split_once('-')
                .ok_or_else(|| format!("invalid range in {s:?}: expected START-END"))?;
            let (start, end) = (parse_time(a)?, parse_time(b)?);
            if start >= end {
                return Err(format!("invalid range in {s:?}: start must be before end"));
            }
            (clip, Some((start, end)))
        }
        None => (s, None),
    };
    if clip.is_empty() {
        return Err(format!("missing clip name in {s:?}"));
    }
    Ok(RangeSpec {
        text: s.to_owned(),
        clip: clip.to_owned(),
        range,
    })
}

/// Parse a `--durations` entry: `CLIP=SECONDS`.
pub fn parse_clip_duration(s: &str) -> Result<ClipDuration, String> {
    let (clip, d) = s
        .rsplit_once('=')
        .ok_or_else(|| format!("invalid duration {s:?}: expected CLIP=SECONDS"))?;
    let duration = parse_time(d)?;
    if clip.is_empty() || duration.0 <= 0 {
        return Err(format!(
            "invalid duration {s:?}: expected CLIP=SECONDS with a positive length"
        ));
    }
    Ok(ClipDuration {
        clip: clip.to_owned(),
        duration,
    })
}

/// Parse `--size`: `WIDTHxHEIGHT` in pixels, e.g. `1920x1080`. Whether the
/// encoder accepts it (even, not too large) is checked with the other export
/// settings.
pub fn parse_size(s: &str) -> Result<(u32, u32), String> {
    let bad = || format!("invalid size {s:?}: expected WIDTHxHEIGHT like 1920x1080");
    let (w, h) = s.trim().split_once(['x', 'X']).ok_or_else(bad)?;
    let dim = |v: &str| {
        v.parse::<u32>()
            .ok()
            .filter(|&n| n > 0 && v.bytes().all(|c| c.is_ascii_digit()))
    };
    dim(w).zip(dim(h)).ok_or_else(bad)
}

/// Parse `--fps`: a whole number (`30`) or a fraction (`30000/1001`).
pub fn parse_fps(s: &str) -> Result<Rational, String> {
    let bad = || format!("invalid frame rate {s:?}: expected a number like 30 or 30000/1001");
    let t = s.trim();
    let (num, den) = t.split_once('/').unwrap_or((t, "1"));
    let int = |v: &str| {
        v.parse::<i32>()
            .ok()
            .filter(|&n| n > 0 && v.bytes().all(|c| c.is_ascii_digit()))
    };
    let (num, den) = int(num).zip(int(den)).ok_or_else(bad)?;
    if f64::from(num) / f64::from(den) > 1000.0 {
        return Err(bad());
    }
    Ok(Rational::new(num, den))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn us(secs: f64) -> Micros {
        Micros::from_secs_f64(secs)
    }

    #[test]
    fn times() {
        assert_eq!(parse_time("12.5"), Ok(us(12.5)));
        assert_eq!(parse_time("12.5s"), Ok(us(12.5)));
        assert_eq!(parse_time("30s"), Ok(us(30.0)));
        assert_eq!(parse_time("0"), Ok(us(0.0)));
        assert_eq!(parse_time("1m30s"), Ok(us(90.0)));
        assert_eq!(parse_time("1m30.5"), Ok(us(90.5)));
        assert_eq!(parse_time("2m"), Ok(us(120.0)));
        assert_eq!(parse_time("1:30"), Ok(us(90.0)));
        assert_eq!(parse_time("0:02.25"), Ok(us(2.25)));
        for bad in [
            "", "s", "abc", "-1", "1e3", "1:", ":30", "1.5m", "inf", "NaN", "1:2:3",
        ] {
            assert!(parse_time(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn lengths() {
        assert_eq!(parse_length("full"), Ok(Length::FullSong));
        assert_eq!(parse_length("FULL"), Ok(Length::FullSong));
        assert_eq!(parse_length("30s"), Ok(Length::Fixed(us(30.0))));
        assert!(parse_length("0").is_err());
        assert!(parse_length("thirty").is_err());
    }

    #[test]
    fn pins_and_excludes() {
        let p = parse_pin("clip.mp4@12.5-15.0").unwrap();
        assert_eq!(p.clip, "clip.mp4");
        assert_eq!(p.range, Some((us(12.5), us(15.0))));
        // The last '@' separates the range, so clip names may contain '@'.
        let p = parse_pin("a@b.mp4@1-2").unwrap();
        assert_eq!(p.clip, "a@b.mp4");
        assert!(parse_pin("clip.mp4").is_err());
        assert!(parse_pin("clip.mp4@3-2").is_err());
        assert!(parse_pin("clip.mp4@3").is_err());
        assert!(parse_pin("@1-2").is_err());

        let e = parse_exclude("dir/clip.mp4").unwrap();
        assert_eq!((e.clip.as_str(), e.range), ("dir/clip.mp4", None));
        let e = parse_exclude("clip.mp4@0:01-0:02").unwrap();
        assert_eq!(e.range, Some((us(1.0), us(2.0))));
    }

    #[test]
    fn durations() {
        assert_eq!(
            parse_clip_duration("a.mp4=12.5"),
            Ok(ClipDuration {
                clip: "a.mp4".into(),
                duration: us(12.5)
            })
        );
        assert!(parse_clip_duration("a.mp4").is_err());
        assert!(parse_clip_duration("a.mp4=0").is_err());
        assert!(parse_clip_duration("=3").is_err());
    }

    #[test]
    fn sizes() {
        assert_eq!(parse_size("1920x1080"), Ok((1920, 1080)));
        assert_eq!(parse_size("320X180"), Ok((320, 180)));
        for bad in [
            "",
            "1920",
            "1920x",
            "x1080",
            "0x10",
            "-2x4",
            "1.5x2",
            "1920*1080",
        ] {
            assert!(parse_size(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn frame_rates() {
        assert_eq!(parse_fps("30"), Ok(Rational::new(30, 1)));
        assert_eq!(parse_fps("30000/1001"), Ok(Rational::new(30000, 1001)));
        for bad in ["", "0", "29.97", "30/0", "/2", "-30", "5000"] {
            assert!(parse_fps(bad).is_err(), "{bad:?}");
        }
    }
}
