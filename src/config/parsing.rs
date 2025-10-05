use serde::{Deserialize, Deserializer, Serializer, de::Error as DeError};
use std::time::Duration;
use anyhow::{anyhow, Result};

/// Deserialize human-readable sizes like "64MB", "1GB", "512KB"
pub(crate) fn deserialize_size<'de, D>(deserializer: D) -> Result<usize, D::Error>
where
    D: Deserializer<'de>,
{
    let s: String = Deserialize::deserialize(deserializer)?;
    parse_size(&s).map_err(D::Error::custom)
}

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

/// Serialize size to human-readable format
pub(crate) fn serialize_size<S>(size: &usize, serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    let s = format_size(*size);
    serializer.serialize_str(&s)
}

/// Parse human-readable size strings to bytes
pub(crate) fn parse_size(s: &str) -> Result<usize> {
    let s = s.trim().to_lowercase();

    if let Ok(bytes) = s.parse::<usize>() {
        return Ok(bytes);
    }

    let (number_part, suffix) = if s.ends_with("kb") {
        (s.trim_end_matches("kb"), 1024)
    } else if s.ends_with("mb") {
        (s.trim_end_matches("mb"), 1024 * 1024)
    } else if s.ends_with("gb") {
        (s.trim_end_matches("gb"), 1024 * 1024 * 1024)
    } else if s.ends_with("k") {
        (s.trim_end_matches("k"), 1024)
    } else if s.ends_with("m") {
        (s.trim_end_matches("m"), 1024 * 1024)
    } else if s.ends_with("g") {
        (s.trim_end_matches("g"), 1024 * 1024 * 1024)
    } else {
        return Err(anyhow!("Invalid size format: {}. Expected format like '64MB', '1GB', '512KB'", s));
    };

    let number: f64 = number_part.parse()
        .map_err(|_| anyhow!("Invalid number in size: {}", s))?;

    if number < 0.0 {
        return Err(anyhow!("Size cannot be negative: {}", s));
    }

    Ok((number * suffix as f64) as usize)
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
        (s.trim_end_matches("us").trim_end_matches("μs"), Duration::from_micros(1))
    } else if s.ends_with("ms") {
        (s.trim_end_matches("ms"), Duration::from_millis(1))
    } else if s.ends_with("s") {
        (s.trim_end_matches("s"), Duration::from_secs(1))
    } else if s.ends_with("m") {
        (s.trim_end_matches("m"), Duration::from_secs(60))
    } else if s.ends_with("h") {
        (s.trim_end_matches("h"), Duration::from_secs(3600))
    } else {
        return Err(anyhow!("Invalid duration format: {}. Expected format like '10s', '500ms', '1m'", s));
    };

    let number: f64 = number_part.parse()
        .map_err(|_| anyhow!("Invalid number in duration: {}", s))?;

    if number < 0.0 {
        return Err(anyhow!("Duration cannot be negative: {}", s));
    }

    Ok(Duration::from_nanos((number * suffix.as_nanos() as f64) as u64))
}

/// Format Duration to human-readable string
pub(crate) fn format_duration(duration: &Duration) -> String {
    let total_secs = duration.as_secs();
    let nanos = duration.subsec_nanos();

    if total_secs >= 3600 {
        format!("{}h", total_secs / 3600)
    } else if total_secs >= 60 {
        format!("{}m", total_secs / 60)
    } else if total_secs > 0 {
        format!("{}s", total_secs)
    } else if nanos >= 1_000_000 {
        format!("{}ms", nanos / 1_000_000)
    } else if nanos >= 1_000 {
        format!("{}us", nanos / 1_000)
    } else {
        format!("{}ns", nanos)
    }
}

/// Format size to human-readable string
pub(crate) fn format_size(size: usize) -> String {
    const GB: usize = 1024 * 1024 * 1024;
    const MB: usize = 1024 * 1024;
    const KB: usize = 1024;

    if size >= GB && size % GB == 0 {
        format!("{}GB", size / GB)
    } else if size >= MB && size % MB == 0 {
        format!("{}MB", size / MB)
    } else if size >= KB && size % KB == 0 {
        format!("{}KB", size / KB)
    } else {
        size.to_string()
    }
}
