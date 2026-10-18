//! Go-compatible duration strings ("10s", "1m30s", "500ms", "1.5h"), so
//! process.toml files written for the Go version keep working unchanged, and
//! console messages print durations the same way ("1m0s").

use std::fmt::Write;
use std::time::Duration;

pub fn parse(input: &str) -> Result<Duration, String> {
    let invalid = || format!("invalid duration {input:?} (expected e.g. \"10s\", \"1m30s\", \"500ms\")");

    if input == "0" {
        return Ok(Duration::ZERO);
    }
    if input.starts_with('-') {
        return Err(format!("invalid duration {input:?}: must not be negative"));
    }
    let mut rest = input.strip_prefix('+').unwrap_or(input);
    if rest.is_empty() {
        return Err(invalid());
    }

    let mut nanos = 0f64;
    while !rest.is_empty() {
        let num_len = rest
            .find(|c: char| !(c.is_ascii_digit() || c == '.'))
            .unwrap_or(rest.len());
        let value: f64 = rest[..num_len].parse().map_err(|_| invalid())?;
        rest = &rest[num_len..];

        let unit_len = rest
            .find(|c: char| c.is_ascii_digit() || c == '.')
            .unwrap_or(rest.len());
        let factor = match &rest[..unit_len] {
            "ns" => 1.0,
            "us" | "µs" | "μs" => 1e3,
            "ms" => 1e6,
            "s" => 1e9,
            "m" => 60e9,
            "h" => 3600e9,
            "" => return Err(format!("invalid duration {input:?}: missing unit")),
            unit => return Err(format!("invalid duration {input:?}: unknown unit {unit:?}")),
        };
        nanos += value * factor;
        rest = &rest[unit_len..];
    }

    Ok(Duration::from_nanos(nanos.round() as u64))
}

/// Formats like Go's time.Duration.String(): "0s", "500ms", "2s", "1m0s", "1h2m3.5s".
pub fn format(d: Duration) -> String {
    let nanos = d.as_nanos();
    if nanos == 0 {
        return "0s".into();
    }
    if nanos < 1_000 {
        return format!("{nanos}ns");
    }
    if nanos < 1_000_000 {
        return format!("{}µs", fraction(nanos, 1_000, 3));
    }
    if nanos < 1_000_000_000 {
        return format!("{}ms", fraction(nanos, 1_000_000, 6));
    }

    let secs = d.as_secs();
    let (h, m, s) = (secs / 3600, secs % 3600 / 60, secs % 60);
    let mut out = String::new();
    if h > 0 {
        let _ = write!(out, "{h}h{m}m");
    } else if m > 0 {
        let _ = write!(out, "{m}m");
    }
    let sub = u128::from(s) * 1_000_000_000 + u128::from(d.subsec_nanos());
    let _ = write!(out, "{}s", fraction(sub, 1_000_000_000, 9));
    out
}

fn fraction(value: u128, unit: u128, digits: usize) -> String {
    let (whole, rem) = (value / unit, value % unit);
    if rem == 0 {
        return whole.to_string();
    }
    let frac = format!("{rem:0digits$}");
    format!("{whole}.{}", frac.trim_end_matches('0'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_go_syntax() {
        assert_eq!(parse("10s").unwrap(), Duration::from_secs(10));
        assert_eq!(parse("1m30s").unwrap(), Duration::from_secs(90));
        assert_eq!(parse("500ms").unwrap(), Duration::from_millis(500));
        assert_eq!(parse("1.5h").unwrap(), Duration::from_secs(5400));
        assert_eq!(parse("2h45m").unwrap(), Duration::from_secs(9900));
        assert_eq!(parse("0").unwrap(), Duration::ZERO);
        assert_eq!(parse("10µs").unwrap(), Duration::from_micros(10));
    }

    #[test]
    fn rejects_garbage() {
        for bad in ["", "10", "s", "-1s", "1x", "1.2.3s", "10 s"] {
            assert!(parse(bad).is_err(), "{bad:?} should be rejected");
        }
    }

    #[test]
    fn formats_like_go() {
        assert_eq!(format(Duration::ZERO), "0s");
        assert_eq!(format(Duration::from_secs(2)), "2s");
        assert_eq!(format(Duration::from_secs(60)), "1m0s");
        assert_eq!(format(Duration::from_secs(3723)), "1h2m3s");
        assert_eq!(format(Duration::from_millis(1500)), "1.5s");
        assert_eq!(format(Duration::from_millis(500)), "500ms");
        assert_eq!(format(Duration::from_secs(3600)), "1h0m0s");
    }
}
