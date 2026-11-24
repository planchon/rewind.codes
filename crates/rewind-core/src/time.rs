use std::time::Duration;

/// Milliseconds since the Unix epoch, UTC. The journal's only notion of time.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Timestamp(pub u64);

impl Timestamp {
    pub const fn from_millis(ms: u64) -> Self {
        Self(ms)
    }

    pub const fn as_millis(self) -> u64 {
        self.0
    }

    pub fn saturating_add(self, duration: Duration) -> Self {
        let ms = u64::try_from(duration.as_millis()).unwrap_or(u64::MAX);
        Self(self.0.saturating_add(ms))
    }

    /// Time elapsed since `earlier`, zero if `earlier` is later.
    pub fn saturating_since(self, earlier: Timestamp) -> Duration {
        Duration::from_millis(self.0.saturating_sub(earlier.0))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DurationError {
    #[error("empty duration")]
    Empty,
    #[error("invalid number at {0:?}")]
    InvalidNumber(String),
    #[error("missing unit after {0:?} (expected ms, s, m, h, d or w)")]
    MissingUnit(String),
    #[error("unknown unit {0:?} (expected ms, s, m, h, d or w)")]
    UnknownUnit(String),
    #[error("duration is too large")]
    Overflow,
}

/// Parses a duration the way workflows write them: `"500ms"`, `"10s"`, `"1h30m"`, `"1.5d"`, `"2w"`.
///
/// Components may be separated by whitespace (`"1h 30m"`). The result is rounded to the
/// millisecond. A bare number has no unit and is rejected: callers that accept a number of
/// milliseconds handle it themselves.
pub fn parse_duration(input: &str) -> Result<Duration, DurationError> {
    let mut rest = input.trim();
    if rest.is_empty() {
        return Err(DurationError::Empty);
    }

    let mut total_ms = 0.0_f64;
    while !rest.is_empty() {
        let number_len = rest
            .find(|c: char| !(c.is_ascii_digit() || c == '.'))
            .unwrap_or(rest.len());
        let (number, after_number) = rest.split_at(number_len);
        let value: f64 = number
            .parse()
            .map_err(|_| DurationError::InvalidNumber(rest.to_owned()))?;

        let unit_len = after_number
            .find(|c: char| !c.is_ascii_alphabetic())
            .unwrap_or(after_number.len());
        let (unit, after_unit) = after_number.split_at(unit_len);
        let unit_ms = match unit {
            "ms" => 1.0,
            "s" => 1_000.0,
            "m" => 60_000.0,
            "h" => 3_600_000.0,
            "d" => 86_400_000.0,
            "w" => 604_800_000.0,
            "" => return Err(DurationError::MissingUnit(number.to_owned())),
            other => return Err(DurationError::UnknownUnit(other.to_owned())),
        };

        total_ms += value * unit_ms;
        rest = after_unit.trim_start();
    }

    if !total_ms.is_finite() || total_ms >= u64::MAX as f64 {
        return Err(DurationError::Overflow);
    }
    Ok(Duration::from_millis(total_ms.round() as u64))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(input: &str) -> u64 {
        parse_duration(input).unwrap().as_millis() as u64
    }

    #[test]
    fn parses_single_units() {
        assert_eq!(ms("500ms"), 500);
        assert_eq!(ms("10s"), 10_000);
        assert_eq!(ms("5m"), 300_000);
        assert_eq!(ms("2h"), 7_200_000);
        assert_eq!(ms("10d"), 864_000_000);
        assert_eq!(ms("1w"), 604_800_000);
    }

    #[test]
    fn parses_compound_and_fractional() {
        assert_eq!(ms("1h30m"), 5_400_000);
        assert_eq!(ms(" 1h 30m 15s "), 5_415_000);
        assert_eq!(ms("1.5d"), 129_600_000);
        assert_eq!(ms("0.5ms"), 1);
    }

    #[test]
    fn rejects_malformed_input() {
        assert_eq!(parse_duration("  "), Err(DurationError::Empty));
        assert_eq!(
            parse_duration("10"),
            Err(DurationError::MissingUnit("10".into()))
        );
        assert_eq!(
            parse_duration("10 d"),
            Err(DurationError::MissingUnit("10".into()))
        );
        assert_eq!(
            parse_duration("3y"),
            Err(DurationError::UnknownUnit("y".into()))
        );
        assert!(matches!(
            parse_duration("-5s"),
            Err(DurationError::InvalidNumber(_))
        ));
        assert!(matches!(
            parse_duration("1.2.3s"),
            Err(DurationError::InvalidNumber(_))
        ));
        assert!(matches!(
            parse_duration("h"),
            Err(DurationError::InvalidNumber(_))
        ));
        assert_eq!(
            parse_duration("1e30w"),
            Err(DurationError::UnknownUnit("e".into()))
        );
        assert_eq!(
            parse_duration("99999999999999999999999w"),
            Err(DurationError::Overflow)
        );
    }

    #[test]
    fn timestamps_saturate() {
        let t = Timestamp::from_millis(1_000);
        assert_eq!(t.saturating_add(Duration::from_secs(1)), Timestamp(2_000));
        assert_eq!(
            Timestamp(u64::MAX - 1).saturating_add(Duration::from_secs(1)),
            Timestamp(u64::MAX)
        );
        assert_eq!(
            Timestamp(5_000).saturating_since(Timestamp(2_000)),
            Duration::from_secs(3)
        );
        assert_eq!(
            Timestamp(1_000).saturating_since(Timestamp(2_000)),
            Duration::ZERO
        );
    }
}
