use anyhow::{anyhow, Result};
use serde::{de::Error as DeError, Deserialize, Deserializer, Serializer};
use std::time::Duration;

/// Deserialize human-readable durations like "10s", "500ms", "1m"
pub(crate) fn deserialize_duration<'de, D>(deserializer: D) -> Result<Duration, D::Error>
where
    D: Deserializer<'de>,
{
    let s: String = Deserialize::deserialize(deserializer)?;
    parse_duration(&s).map_err(D::Error::custom)
}

/// Serialize Duration to human-readable format
pub(crate) fn serialize_duration<S>(duration: &Duration, serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    let s = format_duration(duration);
    serializer.serialize_str(&s)
}

/// Deserialize optional human-readable duration
pub(crate) fn deserialize_duration_opt<'de, D>(
    deserializer: D,
) -> Result<Option<Duration>, D::Error>
where
    D: Deserializer<'de>,
{
    let opt: Option<String> = Option::deserialize(deserializer)?;
    match opt {
        Some(s) => parse_duration(&s).map(Some).map_err(D::Error::custom),
        None => Ok(None),
    }
}

/// Serialize optional Duration to human-readable format
pub(crate) fn serialize_duration_opt<S>(
    duration: &Option<Duration>,
    serializer: S,
) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    match duration {
        Some(d) => serializer.serialize_some(&format_duration(d)),
        None => serializer.serialize_none(),
    }
}

/// Parse human-readable duration strings to Duration
pub fn parse_duration(s: &str) -> Result<Duration> {
    let s = s.trim().to_lowercase();

    if let Ok(secs) = s.parse::<u64>() {
        return Ok(Duration::from_secs(secs));
    }

    let (number_part, suffix) = if s.ends_with("ns") {
        (s.trim_end_matches("ns"), Duration::from_nanos(1))
    } else if s.ends_with("us") || s.ends_with("μs") {
        (
            s.trim_end_matches("us").trim_end_matches("μs"),
            Duration::from_micros(1),
        )
    } else if s.ends_with("ms") {
        (s.trim_end_matches("ms"), Duration::from_millis(1))
    } else if s.ends_with("s") {
        (s.trim_end_matches("s"), Duration::from_secs(1))
    } else if s.ends_with("m") {
        (s.trim_end_matches("m"), Duration::from_secs(60))
    } else if s.ends_with("h") {
        (s.trim_end_matches("h"), Duration::from_secs(3600))
    } else {
        return Err(anyhow!(
            "Invalid duration format: {}. Expected format like '10s', '500ms', '1m'",
            s
        ));
    };

    let number: f64 = number_part
        .parse()
        .map_err(|_| anyhow!("Invalid number in duration: {}", s))?;

    if number < 0.0 {
        return Err(anyhow!("Duration cannot be negative: {}", s));
    }

    Ok(Duration::from_nanos(
        (number * suffix.as_nanos() as f64) as u64,
    ))
}

/// Format Duration to human-readable string, preserving sub-unit remainders.
/// e.g. 90s → "1m30s", 3661s → "1h1m1s"
pub(crate) fn format_duration(duration: &Duration) -> String {
    let total_secs = duration.as_secs();
    let nanos = duration.subsec_nanos();

    if total_secs == 0 {
        if nanos >= 1_000_000 {
            return format!("{}ms", nanos / 1_000_000);
        } else if nanos >= 1_000 {
            return format!("{}us", nanos / 1_000);
        } else {
            return format!("{}ns", nanos);
        }
    }

    let mut result = String::new();
    let hours = total_secs / 3600;
    let minutes = (total_secs % 3600) / 60;
    let secs = total_secs % 60;

    if hours > 0 {
        result.push_str(&format!("{}h", hours));
    }
    if minutes > 0 {
        result.push_str(&format!("{}m", minutes));
    }
    if secs > 0 || result.is_empty() {
        result.push_str(&format!("{}s", secs));
    }
    result
}
