//! Opt-in structured access log (`--access-log <PATH>`, `-` = stdout).
//!
//! One JSON line per completed HTTP response. Disabled costs the hot path a
//! single relaxed load per request. Enabled, the request fields are copied
//! out at parse time (the request buffer is reused for the response phase),
//! and the line is written into its worker thread's pending buffer. A worker
//! hands its buffer to the writer task as it parks, or once it holds
//! `HANDOFF` bytes: an idle worker's lines go at once, a busy one's by size,
//! none on a clock, and the writer is woken once per buffer, not per line.
//! The data path never blocks on the log sink: past `QUEUED_MAX` bytes
//! queued, a buffer is dropped and its lines counted in
//! `portail_access_log_dropped_total`. Lines keep their order per worker
//! thread only: a connection's requests may run on different workers, so
//! even its lines can arrive out of order; each carries its timestamp. At
//! shutdown, [`finish`] waits for the workers' lines and their write.

use std::borrow::Cow;
use std::cell::RefCell;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::Relaxed, Ordering::SeqCst};
use std::sync::OnceLock;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

use crate::logging::warn;

static ENABLED: AtomicBool = AtomicBool::new(false);
static SENDER: OnceLock<UnboundedSender<Message>> = OnceLock::new();
/// Bytes handed to the writer and not yet taken by it.
static QUEUED: AtomicUsize = AtomicUsize::new(0);
/// Bytes in the workers' pending buffers.
static PENDING_BYTES: AtomicUsize = AtomicUsize::new(0);
/// Set by [`finish`]: from then on a hand-off that empties the workers'
/// buffers wakes it. Unset, a hand-off pays one load for it.
static FINISHING: AtomicBool = AtomicBool::new(false);
static HANDED: OnceLock<tokio::sync::Notify> = OnceLock::new();

/// What the writer receives.
enum Message {
    Lines(String),
    /// Answered once everything sent before it is written and flushed.
    Finished(tokio::sync::oneshot::Sender<()>),
}

/// Bytes queued towards the writer before drops start. Sized for a burst,
/// not sustained overload: a sink slower than the request rate must shed
/// lines, never backpressure the data path.
const QUEUED_MAX: usize = 2 * 1024 * 1024;

/// A worker hands its pending lines over once they reach this many bytes.
const HANDOFF: usize = 16 * 1024;

thread_local! {
    /// The lines this worker thread wrote and has not handed over yet.
    static PENDING: RefCell<String> = const { RefCell::new(String::new()) };
}

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

/// Write one log line for a completed response into this thread's pending
/// buffer. `backend` is None for responses the proxy answered itself
/// (redirects, direct errors).
pub fn emit(cap: Option<Box<AccessCapture>>, status: u16, backend: Option<SocketAddr>) {
    let Some(cap) = cap else { return };
    let Some(tx) = SENDER.get() else { return };
    let dur_us = u64::try_from(cap.start.elapsed().as_micros()).unwrap_or(u64::MAX);
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    PENDING.with_borrow_mut(|pending| {
        let before = pending.len();
        write_line(
            pending,
            &cap,
            status,
            backend,
            ts.as_secs(),
            ts.subsec_millis(),
            dur_us,
        );
        PENDING_BYTES.fetch_add(pending.len() - before, Relaxed);
        if pending.len() >= HANDOFF {
            hand_off(tx, pending);
        }
    });
}

/// Hand over this thread's lines, wait for every worker's (each hands its
/// over as it parks, and after the drain they are idle), then for the
/// writer to have written and flushed them all. Called once, after the
/// drain: the last requests' lines must not be lost to the exit.
pub async fn finish() {
    let Some(tx) = SENDER.get() else { return };
    FINISHING.store(true, SeqCst);
    flush();
    let handed = HANDED.get_or_init(tokio::sync::Notify::new);
    loop {
        let notified = handed.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if PENDING_BYTES.load(SeqCst) == 0 {
            break;
        }
        notified.await;
    }
    let (written, done) = tokio::sync::oneshot::channel();
    if tx.send(Message::Finished(written)).is_ok() {
        let _ = done.await;
    }
}

/// Hand this thread's pending lines to the writer. The runtime calls it as
/// a worker parks; a runtime without that hook calls it itself.
pub fn flush() {
    let Some(tx) = SENDER.get() else { return };
    let _ = PENDING.try_with(|cell| {
        if let Ok(mut pending) = cell.try_borrow_mut() {
            if !pending.is_empty() {
                hand_off(tx, &mut pending);
            }
        }
    });
}

fn hand_off(tx: &UnboundedSender<Message>, pending: &mut String) {
    // Sized by what was just handed over: an idle worker that parks after
    // one line keeps a small buffer.
    let capacity = pending.len().min(HANDOFF);
    let lines = std::mem::replace(pending, String::with_capacity(capacity));
    let len = lines.len();
    if QUEUED.fetch_add(len, Relaxed) + len > QUEUED_MAX {
        QUEUED.fetch_sub(len, Relaxed);
        drop_lines(&lines);
    } else if let Err(refused) = tx.send(Message::Lines(lines)) {
        QUEUED.fetch_sub(len, Relaxed);
        if let Message::Lines(lines) = refused.0 {
            drop_lines(&lines);
        }
    }
    if PENDING_BYTES.fetch_sub(len, SeqCst) == len && FINISHING.load(SeqCst) {
        if let Some(handed) = HANDED.get() {
            handed.notify_waiters();
        }
    }
}

fn drop_lines(lines: &str) {
    crate::metrics::METRICS
        .access_log_dropped_total
        .add(lines.matches('\n').count() as u64);
}

fn write_line(
    out: &mut String,
    cap: &AccessCapture,
    status: u16,
    backend: Option<SocketAddr>,
    secs: u64,
    millis: u32,
    dur_us: u64,
) {
    use std::fmt::Write;
    let _ = write!(
        out,
        "{{\"ts\":{}.{:03},\"client\":\"{}\",\"listener\":{},\"tls\":{},\"method\":\"{}\",\"path\":\"{}\",\"host\":\"{}\",\"status\":{},\"backend\":",
        secs,
        millis,
        cap.peer,
        cap.listener,
        cap.tls,
        esc(&cap.method),
        esc(&cap.path),
        esc(&cap.host),
        status,
    );
    match backend {
        Some(b) => {
            let _ = write!(out, "\"{b}\"");
        }
        None => out.push_str("null"),
    }
    let _ = writeln!(out, ",\"duration_us\":{dur_us}}}");
}

#[cfg(test)]
fn format_line(
    cap: &AccessCapture,
    status: u16,
    backend: Option<SocketAddr>,
    secs: u64,
    millis: u32,
    dur_us: u64,
) -> String {
    let mut out = String::new();
    write_line(&mut out, cap, status, backend, secs, millis, dur_us);
    out
}

/// JSON string escaping for the attacker-controlled fields (method, path,
/// Host). Everything else in the line is proxy-generated.
fn esc(s: &str) -> Cow<'_, str> {
    use std::fmt::Write;
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
                let _ = write!(out, "\\u{:04x}", c as u32);
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
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
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
    mut rx: UnboundedReceiver<Message>,
    mut sink: Box<dyn tokio::io::AsyncWrite + Send + Unpin>,
) {
    let mut batch = String::with_capacity(64 * 1024);
    while let Some(first) = rx.recv().await {
        let (mut taken, mut finished) = (0, None);
        let mut next = Some(first);
        while let Some(message) = next.take() {
            match message {
                Message::Lines(lines) => {
                    taken += lines.len();
                    batch.push_str(&lines);
                }
                Message::Finished(written) => finished = Some(written),
            }
            if finished.is_none() && batch.len() < 64 * 1024 {
                next = rx.try_recv().ok();
            }
        }
        QUEUED.fetch_sub(taken, Relaxed);
        if let Err(e) = sink.write_all(batch.as_bytes()).await {
            let lines = batch.matches('\n').count() as u64;
            crate::metrics::METRICS.access_log_dropped_total.add(lines);
            warn!("access log write failed ({} lines lost): {}", lines, e);
        } else {
            let _ = sink.flush().await;
        }
        batch.clear();
        if let Some(written) = finished {
            let _ = written.send(());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A buffer goes to the writer whole; past the byte budget it is
    /// dropped whole, its lines counted, and nothing is queued.
    #[test]
    fn a_buffer_is_handed_over_whole_or_dropped_whole() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let queued = QUEUED.load(Relaxed);
        let mut pending = "a\nb\n".to_string();
        PENDING_BYTES.fetch_add(4, Relaxed);
        hand_off(&tx, &mut pending);
        assert!(pending.is_empty());
        let Ok(Message::Lines(lines)) = rx.try_recv() else {
            panic!("the buffer, whole");
        };
        assert_eq!(lines, "a\nb\n");
        assert_eq!(QUEUED.load(Relaxed), queued + 4);
        QUEUED.fetch_sub(4, Relaxed);

        let dropped = crate::metrics::METRICS.access_log_dropped_total.get();
        QUEUED.fetch_add(QUEUED_MAX, Relaxed);
        let mut pending = "c\nd\ne\n".to_string();
        PENDING_BYTES.fetch_add(6, Relaxed);
        hand_off(&tx, &mut pending);
        QUEUED.fetch_sub(QUEUED_MAX, Relaxed);
        assert!(pending.is_empty());
        assert!(rx.try_recv().is_err(), "over the budget, nothing queued");
        assert_eq!(QUEUED.load(Relaxed), queued);
        assert_eq!(
            crate::metrics::METRICS.access_log_dropped_total.get(),
            dropped + 3
        );
    }

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
