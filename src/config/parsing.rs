use anyhow::{anyhow, Result};
use std::time::Duration;

/// `#[serde(with = "human_duration")]` adapter for human-readable durations
/// like "10s", "500ms", "1m30s". `human_duration::opt` handles
/// `Option<Duration>` fields.
pub(crate) mod human_duration {
    use serde::{de::Error as DeError, Deserialize, Deserializer, Serializer};
    use std::time::Duration;

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Duration, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s: String = Deserialize::deserialize(deserializer)?;
        super::parse_duration(&s).map_err(D::Error::custom)
    }

    pub fn serialize<S>(duration: &Duration, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&super::format_duration(duration))
    }

    pub mod opt {
        use serde::{de::Error as DeError, Deserialize, Deserializer, Serializer};
        use std::time::Duration;

        pub fn deserialize<'de, D>(deserializer: D) -> Result<Option<Duration>, D::Error>
        where
            D: Deserializer<'de>,
        {
            let opt: Option<String> = Option::deserialize(deserializer)?;
            match opt {
                Some(s) => super::super::parse_duration(&s)
                    .map(Some)
                    .map_err(D::Error::custom),
                None => Ok(None),
            }
        }

        pub fn serialize<S>(duration: &Option<Duration>, serializer: S) -> Result<S::Ok, S::Error>
        where
            S: Serializer,
        {
            match duration {
                Some(d) => serializer.serialize_some(&super::super::format_duration(d)),
                None => serializer.serialize_none(),
            }
        }
    }
}

/// Parse human-readable duration strings to Duration: a bare number of
/// seconds, or segments like "1m30s" and "1.5s" (units h, m, s, ms, us,
/// ns), summed. Gateway API durations (GEP-2257) are such segment lists, and
/// [`format_duration`] writes them, so a config survives its JSON form.
pub fn parse_duration(s: &str) -> Result<Duration> {
    let s = s.trim().to_lowercase();

    if let Ok(secs) = s.parse::<u64>() {
        return Ok(Duration::from_secs(secs));
    }
    if s.is_empty() {
        return Err(anyhow!("Empty duration"));
    }

    let mut total = Duration::ZERO;
    let mut rest = s.as_str();
    while !rest.is_empty() {
        let digits = rest
            .find(|c: char| !(c.is_ascii_digit() || c == '.' || c == '-'))
            .ok_or_else(|| anyhow!("Duration segment without a unit: {}", s))?;
        let (number_part, tail) = rest.split_at(digits);
        let (unit, tail) = [
            ("ns", Duration::from_nanos(1)),
            ("us", Duration::from_micros(1)),
            ("μs", Duration::from_micros(1)),
            ("ms", Duration::from_millis(1)),
            ("s", Duration::from_secs(1)),
            ("m", Duration::from_secs(60)),
            ("h", Duration::from_secs(3600)),
        ]
        .into_iter()
        .find_map(|(suffix, unit)| tail.strip_prefix(suffix).map(|tail| (unit, tail)))
        .ok_or_else(|| {
            anyhow!(
                "Invalid duration format: {}. Expected format like '10s', '500ms', '1m'",
                s
            )
        })?;
        let number: f64 = number_part
            .parse()
            .map_err(|_| anyhow!("Invalid number in duration: {}", s))?;
        if number < 0.0 {
            return Err(anyhow!("Duration cannot be negative: {}", s));
        }
        total += Duration::from_nanos((number * unit.as_nanos() as f64) as u64);
        rest = tail;
    }
    Ok(total)
}

/// Format Duration to human-readable string, preserving sub-unit remainders.
/// e.g. 90s -> "1m30s", 3661s -> "1h1m1s", 1.5s -> "1s500ms"
pub(crate) fn format_duration(duration: &Duration) -> String {
    let total_secs = duration.as_secs();
    let nanos = duration.subsec_nanos();

    let mut result = String::new();
    for (value, unit) in [
        (total_secs / 3600, "h"),
        ((total_secs % 3600) / 60, "m"),
        (total_secs % 60, "s"),
        (u64::from(nanos / 1_000_000), "ms"),
        (u64::from(nanos / 1_000 % 1_000), "us"),
        (u64::from(nanos % 1_000), "ns"),
    ] {
        if value > 0 {
            result.push_str(&format!("{}{}", value, unit));
        }
    }
    if result.is_empty() {
        result.push_str("0s");
    }
    result
}
