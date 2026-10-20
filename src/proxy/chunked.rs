//! HTTP/1.1 `Transfer-Encoding: chunked` stream position observer.
//!
//! A [`ChunkedStream`] watches bytes flow through a chunked-encoded HTTP
//! body and reports when the framing has fully terminated — i.e. when the
//! `last-chunk` line and the (possibly empty) trailer-section, followed by
//! the final CRLF, have all passed.
//!
//! The observer is purely passive: it never copies, modifies, or retains
//! references to bytes. Callers are responsible for writing bytes to the
//! wire; this type answers "is the stream over yet?" and "how many of these
//! bytes belong to it?" ([`observe`](ChunkedStream::observe) returns the
//! consumed count). That separation lets the proxy forward chunked bytes
//! verbatim, stop reading from the upstream at the right moment, and keep
//! bytes past the terminator (a pipelined next message) out of the body.
//!
//! ### Why not a pattern match on `0\r\n\r\n`?
//!
//! The naive approach — scan the byte stream for the literal `0\r\n\r\n`
//! sequence — false-positives when those bytes appear as ordinary chunk
//! data. In practice this fires for gRPC-Web success responses, whose
//! in-body trailer frame ends with `grpc-status:0\r\n`; combined with the
//! chunk-end CRLF, the wire shows `:0\r\n\r\n`, which a 5-byte tail
//! matcher cannot distinguish from a legitimate terminator. Result: the
//! proxy stops mid-stream, the real terminator never reaches the client,
//! and the client hangs.
//!
//! This state machine knows where in the framing each byte belongs and so
//! reaches `Done` only at a true chunk-boundary.
//!
//! ### Strictness
//!
//! Bytes are forwarded verbatim to a peer whose parser may be more lenient
//! (bare LF as a line end, oversized sizes wrapped to 64 bits). Wherever two
//! parsers could disagree on where the body ends, the framing turns Broken
//! instead of guessing, and the caller stops forwarding at that byte: past
//! it, a lenient peer could find a second message the proxy never routed.

/// Position in a chunked-encoded body. Bytes are fed in via
/// [`observe`](Self::observe); [`is_terminated`](Self::is_terminated)
/// flips to `true` after the final CRLF of the stream.
#[derive(Debug)]
pub struct ChunkedStream {
    phase: Phase,
}

impl Default for ChunkedStream {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug)]
enum Phase {
    /// Reading hex digits of a chunk-size line. `has_digit` becomes true
    /// once at least one hex digit has been consumed; a CR or `;` before
    /// any digit is malformed.
    Size { hex: u64, has_digit: bool },
    /// Saw `;` in the size line. Skipping `chunk-ext` bytes until CR; a
    /// CTL there (LF, NUL, DEL, ...) is Broken, since a lenient peer may end
    /// the line at it. Carries the parsed size so it survives the extension.
    SizeExt { size: u64 },
    /// Saw the size line's CR. Carries the parsed size so we know
    /// whether to enter `Data` or `TrailerLineStart` when the LF arrives.
    SizeCrlf { size: u64 },
    /// Forwarding chunk-data; `remaining` bytes left until the
    /// chunk-data-end CRLF.
    Data { remaining: u64 },
    /// Chunk-data finished. Expecting the trailing `\r\n`.
    DataCrlf { saw_cr: bool },
    /// At the start of a trailer-section line: past `0\r\n` (last-chunk)
    /// or a trailer field's CRLF. A CRLF here is the blank line that ends
    /// the message.
    TrailerLineStart,
    /// Inside a trailer field line; `saw_colon` once its name has ended.
    TrailerLine { saw_colon: bool },
    /// Saw a trailer line's CR; only LF may follow. `blank` when the line
    /// was empty, so that LF ends the message.
    TrailerCr { blank: bool },
    /// Final CRLF observed. Terminal: further bytes are not consumed.
    Done,
    /// Malformed framing. Terminal: the byte that broke it and every byte
    /// after are never consumed.
    Broken,
}

impl ChunkedStream {
    /// Create an observer positioned at the start of a chunked stream
    /// (expecting the first chunk-size line).
    pub const fn new() -> Self {
        Self {
            phase: Phase::Size {
                hex: 0,
                has_digit: false,
            },
        }
    }

    /// Feed a slice of bytes flowing through the proxy. Returns how many of
    /// them belong to this chunked stream: once the terminator is reached,
    /// the remaining bytes (e.g. a pipelined next message on the same
    /// connection) are NOT consumed, and the caller must not forward them as
    /// body.
    ///
    /// A byte that breaks the framing turns the stream Broken, and the
    /// return value is that byte's index: forward only the bytes before it,
    /// never the rest. Once Broken, every call returns 0. Callers must check
    /// [`is_broken`](Self::is_broken) after every call, since a short count
    /// alone does not tell a terminator from a framing error.
    pub fn observe(&mut self, bytes: &[u8]) -> usize {
        self.advance(bytes, |_| {})
    }

    /// Like [`observe`](Self::observe), same return contract, but appends
    /// the chunk payloads (framing stripped) to `out`. Exists for the h2
    /// bridge, which must re-frame an h1 chunked body as DATA frames; the h1
    /// relay paths keep `observe` and forward wire bytes verbatim. Both
    /// drive the same state machine ([`advance`](Self::advance)); the no-op
    /// closure in `observe` monomorphizes back to the plain scan.
    pub fn decode_into(&mut self, bytes: &[u8], out: &mut Vec<u8>) -> usize {
        self.advance(bytes, |d| out.extend_from_slice(d))
    }

    /// Drive the state machine over `bytes`, handing each run of chunk-data
    /// payload (framing stripped) to `on_data`. Returns bytes consumed.
    fn advance(&mut self, bytes: &[u8], mut on_data: impl FnMut(&[u8])) -> usize {
        let mut i = 0;
        while i < bytes.len() {
            match self.phase {
                Phase::Done => return i,
                // Only reachable on entry: the byte that breaks the framing
                // returns its index below.
                Phase::Broken => return 0,
                _ => {}
            }
            // Fast-path: skip the whole chunk-data run in one shot
            // instead of stepping byte by byte. This is the hot path
            // for large bodies — most bytes never need inspection.
            if let Phase::Data { remaining } = &mut self.phase {
                let take = (*remaining).min((bytes.len() - i) as u64) as usize;
                on_data(&bytes[i..i + take]);
                *remaining -= take as u64;
                i += take;
                if *remaining == 0 {
                    self.phase = Phase::DataCrlf { saw_cr: false };
                }
                continue;
            }
            self.step(bytes[i]);
            if matches!(self.phase, Phase::Broken) {
                return i;
            }
            i += 1;
        }
        i
    }

    /// `true` once the final CRLF has passed; the stream is complete.
    pub fn is_terminated(&self) -> bool {
        matches!(self.phase, Phase::Done)
    }

    /// `true` if the observer has detected malformed framing. The proxy must
    /// stop forwarding at the offending byte and never reuse either side's
    /// connection for this exchange.
    pub fn is_broken(&self) -> bool {
        matches!(self.phase, Phase::Broken)
    }

    fn step(&mut self, byte: u8) {
        // `std::mem::replace` lets us move the phase out and pattern-match
        // by value, which keeps the transitions readable.
        self.phase = match std::mem::replace(&mut self.phase, Phase::Broken) {
            Phase::Size { hex, has_digit } => match byte {
                b'0'..=b'9' => match shift_hex(hex, byte - b'0') {
                    Some(h) => Phase::Size {
                        hex: h,
                        has_digit: true,
                    },
                    None => Phase::Broken,
                },
                b'a'..=b'f' => match shift_hex(hex, byte - b'a' + 10) {
                    Some(h) => Phase::Size {
                        hex: h,
                        has_digit: true,
                    },
                    None => Phase::Broken,
                },
                b'A'..=b'F' => match shift_hex(hex, byte - b'A' + 10) {
                    Some(h) => Phase::Size {
                        hex: h,
                        has_digit: true,
                    },
                    None => Phase::Broken,
                },
                b';' if has_digit => Phase::SizeExt { size: hex },
                b'\r' if has_digit => Phase::SizeCrlf { size: hex },
                _ => Phase::Broken,
            },
            Phase::SizeExt { size } => match byte {
                b'\r' => Phase::SizeCrlf { size },
                // HTAB, SP, VCHAR and obs-text: covers token and
                // quoted-string extension values without tracking quotes.
                b'\t' | b' '..=b'~' | 0x80..=0xff => Phase::SizeExt { size },
                _ => Phase::Broken,
            },
            Phase::SizeCrlf { size } => match byte {
                b'\n' if size == 0 => Phase::TrailerLineStart,
                b'\n' => Phase::Data { remaining: size },
                _ => Phase::Broken,
            },
            // The fast-path in `observe` consumes `Data` bytes in bulk and
            // transitions to `DataCrlf` the moment `remaining` hits zero,
            // so `step` should never be called in this phase.
            Phase::Data { .. } => {
                debug_assert!(false, "Phase::Data must be handled by the fast-path");
                Phase::Broken
            }
            Phase::DataCrlf { saw_cr } => match (saw_cr, byte) {
                (false, b'\r') => Phase::DataCrlf { saw_cr: true },
                (true, b'\n') => Phase::Size {
                    hex: 0,
                    has_digit: false,
                },
                _ => Phase::Broken,
            },
            // Bare LF, bare CR and other CTLs all fall to Broken: a
            // bare-LF-tolerant peer would end the trailer-section early and
            // parse the remainder as a new request.
            Phase::TrailerLineStart => match byte {
                b'\r' => Phase::TrailerCr { blank: true },
                // Leading SP/HTAB is obs-fold; a leading ':' an empty name.
                b' ' | b'\t' | b':' => Phase::Broken,
                _ if is_ctl(byte) => Phase::Broken,
                _ => Phase::TrailerLine { saw_colon: false },
            },
            Phase::TrailerLine { saw_colon } => match byte {
                // A line with no ':' is no field, e.g. a smuggled
                // request-line.
                b'\r' if saw_colon => Phase::TrailerCr { blank: false },
                b':' => Phase::TrailerLine { saw_colon: true },
                b'\t' => Phase::TrailerLine { saw_colon },
                _ if is_ctl(byte) => Phase::Broken,
                _ => Phase::TrailerLine { saw_colon },
            },
            Phase::TrailerCr { blank } => match byte {
                b'\n' if blank => Phase::Done,
                b'\n' => Phase::TrailerLineStart,
                _ => Phase::Broken,
            },
            // Done / Broken are terminal — `observe` short-circuits before
            // reaching `step`, so these arms are unreachable. Listed for
            // exhaustiveness; we collapse to Broken to be safe.
            Phase::Done | Phase::Broken => Phase::Broken,
        };
    }
}

/// Shift a hex accumulator left by one nibble. Returns `None` on overflow.
/// Not `checked_shl`: it only rejects shift amounts >= 64 and would wrap
/// `10000000000000005` to 5 while a peer reads a huge chunk.
#[inline]
fn shift_hex(hex: u64, nibble: u8) -> Option<u64> {
    if hex >> 60 != 0 {
        return None;
    }
    Some((hex << 4) | u64::from(nibble))
}

/// RFC 5234 CTL: 0x00-0x1F and DEL.
#[inline]
fn is_ctl(byte: u8) -> bool {
    byte < 0x20 || byte == 0x7f
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Feed `bytes` byte-by-byte to a fresh observer. Used to verify the
    /// state machine is read-size-invariant.
    fn observe_byte_by_byte(bytes: &[u8]) -> ChunkedStream {
        let mut s = ChunkedStream::new();
        for b in bytes {
            s.observe(&[*b]);
        }
        s
    }

    /// Feed `bytes` in a single slice.
    fn observe_one_shot(bytes: &[u8]) -> ChunkedStream {
        let mut s = ChunkedStream::new();
        s.observe(bytes);
        s
    }

    #[test]
    fn empty_body_terminator_only() {
        // No data chunks — just last-chunk + final CRLF.
        let s = observe_one_shot(b"0\r\n\r\n");
        assert!(s.is_terminated());
        assert!(!s.is_broken());
    }

    #[test]
    fn single_chunk_then_terminator() {
        let s = observe_one_shot(b"5\r\nhello\r\n0\r\n\r\n");
        assert!(s.is_terminated());
    }

    #[test]
    fn multiple_chunks() {
        let s = observe_one_shot(b"5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n");
        assert!(s.is_terminated());
    }

    #[test]
    fn byte_by_byte_feeding() {
        let s = observe_byte_by_byte(b"5\r\nhello\r\n0\r\n\r\n");
        assert!(s.is_terminated());
    }

    #[test]
    fn split_across_arbitrary_reads() {
        let body = b"5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n";
        // Try every possible split point.
        for split in 0..=body.len() {
            let mut s = ChunkedStream::new();
            s.observe(&body[..split]);
            s.observe(&body[split..]);
            assert!(s.is_terminated(), "split at {} did not terminate", split);
        }
    }

    #[test]
    fn chunk_extension_on_size_line() {
        let s = observe_one_shot(b"5;name=val\r\nhello\r\n0\r\n\r\n");
        assert!(s.is_terminated());
    }

    #[test]
    fn chunk_extension_on_last_chunk() {
        let s = observe_one_shot(b"0;foo=bar\r\n\r\n");
        assert!(s.is_terminated());
    }

    #[test]
    fn trailer_section_single_header() {
        // gRPC-Web style: last-chunk, one trailer header, blank line.
        let s = observe_one_shot(b"0\r\nGrpc-Status: 0\r\n\r\n");
        assert!(s.is_terminated());
    }

    #[test]
    fn trailer_section_multiple_headers() {
        let s = observe_one_shot(b"0\r\nGrpc-Status: 0\r\nGrpc-Message: OK\r\n\r\n");
        assert!(s.is_terminated());
    }

    #[test]
    fn hex_size_large_uppercase() {
        // 0x1A = 26 bytes of data.
        let body = b"1A\r\nabcdefghijklmnopqrstuvwxyz\r\n0\r\n\r\n";
        let s = observe_one_shot(body);
        assert!(s.is_terminated());
    }

    #[test]
    fn hex_size_large_lowercase() {
        let body = b"1a\r\nabcdefghijklmnopqrstuvwxyz\r\n0\r\n\r\n";
        let s = observe_one_shot(body);
        assert!(s.is_terminated());
    }

    /// Regression for the bug this module fixes. A gRPC-Web success
    /// response carries an in-body trailer frame ending with
    /// `grpc-status:0\r\n`. Combined with the chunk-end CRLF, the wire
    /// bytes are `:0\r\n\r\n` — which the old 5-byte tail matcher
    /// mistakenly accepted as the chunked terminator. The state machine
    /// must NOT terminate until the real `0\r\n\r\n` arrives.
    #[test]
    fn regression_grpc_web_trailer_does_not_false_terminate() {
        // Construct a single data chunk whose payload ends with
        // `grpc-status:0\r\n` (a 20-byte gRPC-Web trailer frame:
        // 1-byte flag + 4-byte length prefix + 15-byte trailer text).
        let trailer_frame: &[u8] = b"\x80\x00\x00\x00\x0fgrpc-status:0\r\n";
        assert_eq!(trailer_frame.len(), 20);

        let mut response = Vec::new();
        response.extend_from_slice(b"14\r\n"); // chunk size 0x14 = 20
        response.extend_from_slice(trailer_frame);
        response.extend_from_slice(b"\r\n"); // chunk-data-end CRLF
                                             // At this point the wire ends with `:0\r\n\r\n` — the
                                             // false-positive window. Verify the observer is NOT terminated.
        let mut s = ChunkedStream::new();
        s.observe(&response);
        assert!(
            !s.is_terminated(),
            "observer falsely terminated at trailer-frame chunk-end"
        );
        assert!(!s.is_broken());

        // Now send the real terminator and confirm.
        s.observe(b"0\r\n\r\n");
        assert!(s.is_terminated(), "observer missed the real terminator");
    }

    /// Even more adversarial: a chunk's *data* payload contains the
    /// exact bytes `\r\n0\r\n\r\n` somewhere in the middle. A correct
    /// state machine ignores it because we are still in `Data` for that
    /// many bytes.
    #[test]
    fn adversarial_data_contains_terminator_bytes() {
        let data: &[u8] = b"prefix\r\n0\r\n\r\nsuffix";
        let mut body = Vec::new();
        body.extend_from_slice(format!("{:x}\r\n", data.len()).as_bytes());
        body.extend_from_slice(data);
        body.extend_from_slice(b"\r\n0\r\n\r\n");

        let s = observe_one_shot(&body);
        assert!(s.is_terminated());

        // And verify it does NOT terminate before the real terminator.
        let prefix = &body[..body.len() - b"0\r\n\r\n".len()];
        let mut early = ChunkedStream::new();
        early.observe(prefix);
        assert!(!early.is_terminated());
    }

    #[test]
    fn bytes_after_terminator_are_not_consumed() {
        let mut s = ChunkedStream::new();
        assert_eq!(s.observe(b"0\r\n\r\n"), 5);
        assert!(s.is_terminated());
        // A pipelined next response on the same connection must not be
        // consumed (the caller forwards only consumed bytes as body) nor
        // flip us back to non-terminated.
        assert_eq!(s.observe(b"HTTP/1.1 200 OK\r\n\r\n"), 0);
        assert!(s.is_terminated());
    }

    /// The terminator and the start of a pipelined next message arriving in
    /// one read: `observe` must report exactly the stream's own bytes.
    #[test]
    fn consumed_count_stops_at_terminator_mid_buffer() {
        let mut s = ChunkedStream::new();
        let wire = b"5\r\nhello\r\n0\r\n\r\nGET / HTTP/1.1\r\n";
        let body_len = b"5\r\nhello\r\n0\r\n\r\n".len();
        assert_eq!(s.observe(wire), body_len);
        assert!(s.is_terminated());
    }

    /// Neither the byte that breaks the framing nor anything after it is
    /// consumed, so a caller cannot forward bytes a lenient peer might
    /// parse as a second message; later calls consume nothing.
    #[test]
    fn broken_framing_stops_at_offending_byte() {
        let mut s = ChunkedStream::new();
        assert_eq!(s.observe(b"Z\r\ngarbage"), 0);
        assert!(s.is_broken());
        assert_eq!(s.observe(b"more"), 0);

        let mut s = ChunkedStream::new();
        assert_eq!(s.observe(b"5\r\nhello\r\nZ\r\n"), 10);
        assert!(s.is_broken());
        assert_eq!(s.observe(b"0\r\n\r\n"), 0);
        assert!(!s.is_terminated());
    }

    /// Feeds `wire` one-shot and byte-by-byte: both must turn Broken with
    /// the consumed total stopping exactly at the offending byte `at`.
    fn assert_broken_at(wire: &[u8], at: usize) {
        let mut s = ChunkedStream::new();
        assert_eq!(s.observe(wire), at, "one-shot count for {:?}", wire);
        assert!(s.is_broken(), "one-shot not broken: {:?}", wire);

        let mut s = ChunkedStream::new();
        let consumed: usize = wire.iter().map(|b| s.observe(&[*b])).sum();
        assert_eq!(consumed, at, "byte-by-byte count for {:?}", wire);
        assert!(s.is_broken(), "byte-by-byte not broken: {:?}", wire);
        assert!(!s.is_terminated());
    }

    /// Feeds `wire` one-shot and byte-by-byte: both must terminate having
    /// consumed all of it.
    fn assert_terminates(wire: &[u8]) {
        let mut s = ChunkedStream::new();
        assert_eq!(s.observe(wire), wire.len(), "one-shot count for {:?}", wire);
        assert!(s.is_terminated(), "one-shot not terminated: {:?}", wire);

        let s = observe_byte_by_byte(wire);
        assert!(s.is_terminated(), "byte-by-byte not terminated: {:?}", wire);
    }

    /// The u64 accumulator must not wrap: `10000000000000005` would parse
    /// as 5 while a peer reads (or rejects) a 2^64+5 byte chunk.
    #[test]
    fn chunk_size_overflow_is_broken() {
        let wire = b"10000000000000005\r\nhello\r\n0\r\n\r\n";
        assert_broken_at(wire, 16);
        // Leading zeros carry no bits, so they never overflow.
        assert_terminates(b"000000000000000000005\r\nhello\r\n0\r\n\r\n");
    }

    #[test]
    fn chunk_size_max_u64_is_accepted() {
        let mut s = ChunkedStream::new();
        assert_eq!(s.observe(b"ffffffffffffffff\r\nabc"), 21);
        assert!(!s.is_broken());
        assert!(!s.is_terminated());
    }

    /// The verified smuggling vector: SP after the size digits.
    #[test]
    fn whitespace_after_size_digits_is_broken() {
        assert_broken_at(b"5 \r\nhello\r\n0\r\n\r\nGET /smuggled HTTP/1.1\r\n\r\n", 1);
        assert_broken_at(b"5\t;ext\r\nhello\r\n0\r\n\r\n", 1);
    }

    /// A lenient peer may end the size line at any of these, then read the
    /// rest of the extension as chunk data.
    #[test]
    fn ctl_in_chunk_extension_is_broken() {
        assert_broken_at(b"5;a\nhello\r\n0\r\n\r\n", 3);
        assert_broken_at(b"5;a=\0\r\nhello\r\n0\r\n\r\n", 4);
        assert_broken_at(b"5;a=\x7f\r\nhello\r\n0\r\n\r\n", 4);
        assert_broken_at(b"5;a=\x01\r\nhello\r\n0\r\n\r\n", 4);
        assert_broken_at(b"0;a\n\r\n", 3);
    }

    #[test]
    fn chunk_extension_quoted_string_value() {
        assert_terminates(b"5;name=\"quoted value; with\ttab \\\"esc\\\"\"\r\nhello\r\n0\r\n\r\n");
        assert_terminates(b"5;n=\x80\xff ; m\r\nhello\r\n0;last=\"x\"\r\n\r\n");
    }

    /// The verified trailer vector: a bare LF ends the trailer-section for
    /// a lenient peer, which then parses the rest as a second request.
    #[test]
    fn trailer_bare_lf_is_broken() {
        assert_broken_at(b"5\r\nhello\r\n0\r\n\nGET /smuggled2 HTTP/1.1\r\n\r\n", 13);
        assert_broken_at(b"0\r\nX-T: v\nGET /s HTTP/1.1\r\nH: x\r\n\r\n", 9);
    }

    #[test]
    fn trailer_bare_cr_is_broken() {
        assert_broken_at(b"0\r\nX-T: v\rX\r\n\r\n", 10);
        assert_broken_at(b"0\r\n\rX", 4);
        assert_broken_at(b"0\r\nX-T: v\r\n\r\r\n", 12);
    }

    #[test]
    fn trailer_ctl_is_broken() {
        assert_broken_at(b"0\r\nX-T: a\0b\r\n\r\n", 9);
        assert_broken_at(b"0\r\nX-T: a\x7fb\r\n\r\n", 9);
        assert_broken_at(b"0\r\nX\x01T: v\r\n\r\n", 4);
        assert_broken_at(b"0\r\n\x0bX: v\r\n\r\n", 3);
    }

    #[test]
    fn trailer_obs_fold_is_broken() {
        assert_broken_at(b"0\r\nX-T: v\r\n folded\r\n\r\n", 11);
        assert_broken_at(b"0\r\n\tX-T: v\r\n\r\n", 3);
    }

    #[test]
    fn trailer_line_without_colon_is_broken() {
        assert_broken_at(b"0\r\nGET /smuggled HTTP/1.1\r\nHost: x\r\n\r\n", 25);
        assert_broken_at(b"0\r\nX-T: v\r\nnocolon\r\n\r\n", 18);
        assert_broken_at(b"0\r\n: empty-name\r\n\r\n", 3);
    }

    #[test]
    fn trailer_with_htab_terminates() {
        assert_terminates(b"0\r\nX-T:\tv\t1\r\nX-U: a\tb \r\n\r\n");
    }

    /// `decode_into` keeps the payload decoded before the offending byte and
    /// reports that byte's index, like `observe`.
    #[test]
    fn decode_into_broken_stops_at_offending_byte() {
        let mut s = ChunkedStream::new();
        let mut out = Vec::new();
        let wire = b"5\r\nhello\r\n0\r\n\nGET / HTTP/1.1\r\n\r\n";
        assert_eq!(s.decode_into(wire, &mut out), 13);
        assert!(s.is_broken());
        assert_eq!(out, b"hello");
        assert_eq!(s.decode_into(b"more", &mut out), 0);
        assert_eq!(out, b"hello");
    }

    #[test]
    fn malformed_non_hex_size() {
        let mut s = ChunkedStream::new();
        s.observe(b"Z\r\n");
        assert!(s.is_broken());
        assert!(!s.is_terminated());
    }

    #[test]
    fn malformed_missing_lf_after_size_cr() {
        let mut s = ChunkedStream::new();
        s.observe(b"5\rx");
        assert!(s.is_broken());
    }

    #[test]
    fn malformed_size_with_no_digits() {
        // CR before any hex digit.
        let mut s = ChunkedStream::new();
        s.observe(b"\r\n");
        assert!(s.is_broken());
    }

    #[test]
    fn decode_into_strips_framing() {
        let mut s = ChunkedStream::new();
        let mut out = Vec::new();
        let wire = b"5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n";
        assert_eq!(s.decode_into(wire, &mut out), wire.len());
        assert_eq!(out, b"hello world");
        assert!(s.is_terminated());
    }

    #[test]
    fn decode_into_split_at_every_boundary() {
        let wire = b"5\r\nhello\r\n6\r\n world\r\n0\r\nX-T: v\r\n\r\n";
        for split in 0..=wire.len() {
            let mut s = ChunkedStream::new();
            let mut out = Vec::new();
            s.decode_into(&wire[..split], &mut out);
            s.decode_into(&wire[split..], &mut out);
            assert_eq!(out, b"hello world", "split at {}", split);
            assert!(s.is_terminated(), "split at {}", split);
        }
    }

    #[test]
    fn decode_into_stops_at_terminator() {
        let mut s = ChunkedStream::new();
        let mut out = Vec::new();
        let wire = b"1\r\nx\r\n0\r\n\r\nleftover";
        let body_len = wire.len() - b"leftover".len();
        assert_eq!(s.decode_into(wire, &mut out), body_len);
        assert_eq!(out, b"x");
    }
}
