//! Opt-in structured access log (`--access-log <PATH>`, `-` = stdout).
//!
//! One JSON line per completed HTTP response. Disabled costs the hot path a
//! single relaxed load per request. Enabled, the request fields are copied
//! out at parse time (the request buffer is reused for the response phase),
//! the line is assembled off the response's critical await points, and
//! handed to a dedicated writer task through a bounded channel: the data
//! path never blocks on the log sink — a full channel drops the line and
//! counts it in `portail_access_log_dropped_total` instead.

use std::borrow::Cow;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::OnceLock;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use tokio::io::AsyncWriteExt;

use crate::logging::warn;

static ENABLED: AtomicBool = AtomicBool::new(false);
static SENDER: OnceLock<tokio::sync::mpsc::Sender<String>> = OnceLock::new();

/// Lines buffered towards the writer before drops start. Sized for a burst,
/// not sustained overload: a sink slower than the request rate must shed
/// lines, never backpressure the data path.
const CHANNEL_CAPACITY: usize = 8192;

/// Request fields copied out before the buffer is reused for the response.
/// Boxed so the disabled path moves one pointer-sized None around.
pub struct AccessCapture {
    start: Instant,
    peer: IpAddr,
    listener: u16,
    tls: bool,
    method: String,
    path: String,
    host: String,
}

pub fn enabled() -> bool {
    ENABLED.load(Relaxed)
}

/// Copy the logged fields out of a raw request block. Returns None when
/// logging is disabled (the only cost on that path) or the block has no
/// parseable request line.
pub fn capture(
    raw: &[u8],
    start: Instant,
    peer: IpAddr,
    listener: u16,
    tls: bool,
) -> Option<Box<AccessCapture>> {
    if !enabled() {
        return None;
    }
    let line_end = raw.iter().position(|&b| b == b'\r' || b == b'\n')?;
    let mut parts = raw[..line_end].split(|&b| b == b' ');
    let method = String::from_utf8_lossy(parts.next()?).into_owned();
    let path = String::from_utf8_lossy(parts.next().unwrap_or(b"")).into_owned();
    // Headers start past the request line's newline; handing host_header the
    // CRLF remnant would read as the blank line and end the scan at once.
    let headers_start = raw[line_end..]
        .iter()
        .position(|&b| b == b'\n')
        .map(|i| line_end + i + 1)
        .unwrap_or(raw.len());
    Some(Box::new(AccessCapture {
        start,
        peer,
        listener,
        tls,
        method,
        path,
        host: host_header(&raw[headers_start..]).unwrap_or_default(),
    }))
}

fn host_header(headers: &[u8]) -> Option<String> {
    for line in headers.split(|&b| b == b'\n') {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if line.is_empty() {
            return None;
        }
        if line.len() > 5 && line[..5].eq_ignore_ascii_case(b"host:") {
            return Some(String::from_utf8_lossy(&line[5..]).trim().to_string());
        }
    }
    None
}

/// Queue one log line for a completed response. `backend` is None for
/// responses the proxy answered itself (redirects, direct errors).
pub fn emit(cap: Option<Box<AccessCapture>>, status: u16, backend: Option<SocketAddr>) {
    let Some(cap) = cap else { return };
    let Some(tx) = SENDER.get() else { return };
    let dur_us = u64::try_from(cap.start.elapsed().as_micros()).unwrap_or(u64::MAX);
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let line = format_line(
        &cap,
        status,
        backend,
        ts.as_secs(),
        ts.subsec_millis(),
        dur_us,
    );
    if tx.try_send(line).is_err() {
        crate::metrics::METRICS.access_log_dropped_total.inc();
    }
}

fn format_line(
    cap: &AccessCapture,
    status: u16,
    backend: Option<SocketAddr>,
    secs: u64,
    millis: u32,
    dur_us: u64,
) -> String {
    let backend = match backend {
        Some(b) => Cow::Owned(format!("\"{}\"", b)),
        None => Cow::Borrowed("null"),
    };
    format!(
        "{{\"ts\":{}.{:03},\"client\":\"{}\",\"listener\":{},\"tls\":{},\"method\":\"{}\",\"path\":\"{}\",\"host\":\"{}\",\"status\":{},\"backend\":{},\"duration_us\":{}}}\n",
        secs,
        millis,
        cap.peer,
        cap.listener,
        cap.tls,
        esc(&cap.method),
        esc(&cap.path),
        esc(&cap.host),
        status,
        backend,
        dur_us,
    )
}

/// JSON string escaping for the attacker-controlled fields (method, path,
/// Host). Everything else in the line is proxy-generated.
fn esc(s: &str) -> Cow<'_, str> {
    if !s
        .bytes()
        .any(|b| b == b'"' || b == b'\\' || b < 0x20 || b == 0x7f)
    {
        return Cow::Borrowed(s);
    }
    let mut out = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if (c as u32) < 0x20 || c == '\u{7f}' => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => out.push(c),
        }
    }
    Cow::Owned(out)
}

/// Open the sink and start the writer task. Must run inside the runtime.
/// Call once at startup, before listeners accept traffic.
pub fn init(target: &str) -> anyhow::Result<()> {
    let sink: Box<dyn tokio::io::AsyncWrite + Send + Unpin> = if target == "-" {
        Box::new(tokio::io::stdout())
    } else {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(target)
            .map_err(|e| anyhow::anyhow!("failed to open access log {}: {}", target, e))?;
        Box::new(tokio::fs::File::from_std(file))
    };
    let (tx, rx) = tokio::sync::mpsc::channel(CHANNEL_CAPACITY);
    SENDER
        .set(tx)
        .map_err(|_| anyhow::anyhow!("access log initialized twice"))?;
    ENABLED.store(true, Relaxed);
    tokio::spawn(writer(rx, sink));
    Ok(())
}

/// Drain the channel into batched writes, flushing whenever the queue goes
/// quiet so lines are visible promptly without a per-line syscall under load.
async fn writer(
    mut rx: tokio::sync::mpsc::Receiver<String>,
    mut sink: Box<dyn tokio::io::AsyncWrite + Send + Unpin>,
) {
    let mut batch = String::with_capacity(16 * 1024);
    while let Some(line) = rx.recv().await {
        batch.push_str(&line);
        while batch.len() < 64 * 1024 {
            match rx.try_recv() {
                Ok(more) => batch.push_str(&more),
                Err(_) => break,
            }
        }
        if let Err(e) = sink.write_all(batch.as_bytes()).await {
            let lines = batch.matches('\n').count() as u64;
            crate::metrics::METRICS.access_log_dropped_total.add(lines);
            warn!("access log write failed ({} lines lost): {}", lines, e);
        } else {
            let _ = sink.flush().await;
        }
        batch.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cap(method: &str, path: &str, host: &str) -> AccessCapture {
        AccessCapture {
            start: Instant::now(),
            peer: "192.0.2.7".parse().unwrap(),
            listener: 443,
            tls: true,
            method: method.into(),
            path: path.into(),
            host: host.into(),
        }
    }

    #[test]
    fn line_shape_and_null_backend() {
        let line = format_line(
            &cap("GET", "/api", "a.example.com"),
            200,
            None,
            1700,
            42,
            1234,
        );
        assert_eq!(
            line,
            "{\"ts\":1700.042,\"client\":\"192.0.2.7\",\"listener\":443,\"tls\":true,\"method\":\"GET\",\"path\":\"/api\",\"host\":\"a.example.com\",\"status\":200,\"backend\":null,\"duration_us\":1234}\n"
        );
    }

    #[test]
    fn attacker_fields_are_escaped() {
        let evil = cap("GE\"T", "/a\\b\u{7f}", "x\r\ny");
        let line = format_line(&evil, 404, "10.0.0.1:80".parse().ok(), 1, 0, 5);
        assert!(line.contains("\"method\":\"GE\\\"T\""));
        assert!(line.contains("\"path\":\"/a\\\\b\\u007f\""));
        assert!(line.contains("\"host\":\"x\\u000d\\u000ay\""));
        assert!(line.contains("\"backend\":\"10.0.0.1:80\""));
        // No raw newline or quote breaks the single-line JSON framing.
        assert_eq!(line.matches('\n').count(), 1);
        assert!(line.ends_with("}\n"));
    }

    #[test]
    fn host_header_scan_stops_at_blank_line() {
        assert_eq!(
            host_header(b"User-Agent: x\r\nHOST:  api.example.com \r\n\r\n"),
            Some("api.example.com".to_string())
        );
        // A Host after the header block (smuggled in a body) is not a header.
        assert_eq!(host_header(b"User-Agent: x\r\n\r\nHost: evil\r\n"), None);
    }

    /// One test owns the ENABLED toggle: splitting the disabled and enabled
    /// assertions across tests would race under the parallel runner.
    #[test]
    fn capture_respects_toggle_and_parses_fields() {
        let raw = b"POST /submit HTTP/1.1\r\nHost: api.example.com\r\n\r\n";
        let peer: IpAddr = "127.0.0.1".parse().unwrap();
        assert!(capture(raw, Instant::now(), peer, 8080, false).is_none());

        ENABLED.store(true, Relaxed);
        let cap = capture(raw, Instant::now(), peer, 8080, false);
        ENABLED.store(false, Relaxed);
        let cap = cap.expect("capture with logging enabled");
        assert_eq!(cap.method, "POST");
        assert_eq!(cap.path, "/submit");
        assert_eq!(cap.host, "api.example.com");
    }
}
