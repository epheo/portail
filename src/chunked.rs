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
    /// Saw `;` in the size line. Skipping `chunk-ext` bytes until CR.
    /// Carries the parsed size so it survives the extension.
    SizeExt { size: u64 },
    /// Saw the size line's CR. Carries the parsed size so we know
    /// whether to enter `Data` or `Trailer` when the LF arrives.
    SizeCrlf { size: u64 },
    /// Forwarding chunk-data; `remaining` bytes left until the
    /// chunk-data-end CRLF.
    Data { remaining: u64 },
    /// Chunk-data finished. Expecting the trailing `\r\n`.
    DataCrlf { saw_cr: bool },
    /// Past `0\r\n` (last-chunk). Scanning for the blank line that ends
    /// the trailer-section. `crlf_pairs` counts consecutive CRLFs;
    /// reaching 2 means we just saw `\r\n\r\n` (the LF of the
    /// last-chunk's CRLF counts as the first pair). Trailer header bytes
    /// reset the run.
    Trailer { crlf_pairs: u8, saw_cr: bool },
    /// Final CRLF observed. Terminal — further bytes are ignored.
    Done,
    /// Malformed framing. Terminal — `is_terminated()` will never report
    /// `true` from here, but we keep accepting bytes so the caller can
    /// continue forwarding without panicking.
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
    /// them belong to this chunked stream — once the terminator is reached,
    /// the remaining bytes (e.g. a pipelined next message on the same
    /// connection) are NOT consumed, and the caller must not forward them as
    /// body. While the framing is `Broken` every byte is reported consumed:
    /// there is no boundary left to respect and the connection is already
    /// unusable for reuse.
    pub fn observe(&mut self, bytes: &[u8]) -> usize {
        let mut i = 0;
        while i < bytes.len() {
            // Terminal phases: Done stops consuming at the boundary; Broken
            // swallows the rest (see doc above).
            match self.phase {
                Phase::Done => return i,
                Phase::Broken => return bytes.len(),
                _ => {}
            }
            // Fast-path: skip the whole chunk-data run in one shot
            // instead of stepping byte by byte. This is the hot path
            // for large bodies — most bytes never need inspection.
            if let Phase::Data { remaining } = &mut self.phase {
                let take = (*remaining).min((bytes.len() - i) as u64);
                *remaining -= take;
                i += take as usize;
                if *remaining == 0 {
                    self.phase = Phase::DataCrlf { saw_cr: false };
                }
                continue;
            }
            self.step(bytes[i]);
            i += 1;
        }
        i
    }

    /// `true` once the final CRLF has passed; the stream is complete.
    pub fn is_terminated(&self) -> bool {
        matches!(self.phase, Phase::Done)
    }

    /// `true` if the observer has detected malformed framing. The proxy
    /// should treat this connection as not reusable.
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
                _ => Phase::SizeExt { size },
            },
            Phase::SizeCrlf { size } => match byte {
                b'\n' if size == 0 => Phase::Trailer {
                    crlf_pairs: 1,
                    saw_cr: false,
                },
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
            Phase::Trailer { crlf_pairs, saw_cr } => match (saw_cr, byte) {
                (false, b'\r') => Phase::Trailer {
                    crlf_pairs,
                    saw_cr: true,
                },
                (true, b'\n') => {
                    let pairs = crlf_pairs + 1;
                    if pairs >= 2 {
                        Phase::Done
                    } else {
                        Phase::Trailer {
                            crlf_pairs: pairs,
                            saw_cr: false,
                        }
                    }
                }
                // Any byte that isn't a CR/LF in sequence is trailer
                // header content; restart the CRLF run.
                _ => Phase::Trailer {
                    crlf_pairs: 0,
                    saw_cr: false,
                },
            },
            // Done / Broken are terminal — `observe` short-circuits before
            // reaching `step`, so these arms are unreachable. Listed for
            // exhaustiveness; we collapse to Broken to be safe.
            Phase::Done | Phase::Broken => Phase::Broken,
        };
    }
}

/// Shift a hex accumulator left by one nibble. Returns `None` on overflow.
#[inline]
fn shift_hex(hex: u64, nibble: u8) -> Option<u64> {
    hex.checked_shl(4)?.checked_add(nibble as u64)
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

    /// Broken framing consumes everything — there is no boundary left, and
    /// the caller keeps forwarding until EOF on a connection that will never
    /// be reused.
    #[test]
    fn broken_framing_consumes_all_bytes() {
        let mut s = ChunkedStream::new();
        assert_eq!(s.observe(b"Z\r\ngarbage"), 10);
        assert!(s.is_broken());
        assert_eq!(s.observe(b"more"), 4);
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
}
