//! Parsing of short human-friendly durations such as `500ms`, `2s`, `15m`, `24h`, `7d`.

use std::fmt;
use std::time::Duration;

/// Error returned when a duration string cannot be parsed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseDurationError(String);

impl fmt::Display for ParseDurationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "invalid duration '{}': expected a whole number followed by ms, s, m, h or d (e.g. 2s, 7d)",
            self.0
        )
    }
}

impl std::error::Error for ParseDurationError {}

/// Parses a duration made of a whole number and a unit suffix
/// (`ms`, `s`, `m`, `h`, `d`). Zero and missing units are rejected.
pub fn parse_duration(input: &str) -> Result<Duration, ParseDurationError> {
    let err = || ParseDurationError(input.to_owned());
    let s = input.trim();
    let split = s.find(|c: char| !c.is_ascii_digit()).ok_or_else(err)?;
    let (digits, unit) = s.split_at(split);
    let value: u64 = digits.parse().map_err(|_| err())?;
    if value == 0 {
        return Err(err());
    }
    let millis_per_unit: u64 = match unit {
        "ms" => 1,
        "s" => 1_000,
        "m" => 60_000,
        "h" => 3_600_000,
        "d" => 86_400_000,
        _ => return Err(err()),
    };
    value
        .checked_mul(millis_per_unit)
        .map(Duration::from_millis)
        .ok_or_else(err)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_each_unit() {
        assert_eq!(parse_duration("500ms"), Ok(Duration::from_millis(500)));
        assert_eq!(parse_duration("2s"), Ok(Duration::from_secs(2)));
        assert_eq!(parse_duration("15m"), Ok(Duration::from_secs(15 * 60)));
        assert_eq!(parse_duration("24h"), Ok(Duration::from_secs(24 * 3600)));
        assert_eq!(parse_duration("7d"), Ok(Duration::from_secs(7 * 86_400)));
    }

    #[test]
    fn trims_whitespace() {
        assert_eq!(parse_duration(" 2s "), Ok(Duration::from_secs(2)));
    }

    #[test]
    fn rejects_invalid_input() {
        for bad in [
            "",
            "s",
            "2",
            "0s",
            "-2s",
            "2x",
            "2 s",
            "1.5s",
            "99999999999999999999d",
        ] {
            assert!(parse_duration(bad).is_err(), "expected error for {bad:?}");
        }
    }
}
