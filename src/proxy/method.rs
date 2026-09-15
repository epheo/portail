//! What an HTTP method looks like on the wire, defined once for the request
//! sniff and the static-config validator so the two can never disagree about
//! which requests portail forwards.

/// Longest method IANA registers is UPDATEREDIRECTREF (17, RFC 4437). The
/// bound is what keeps the sniff's worst case to a handful of byte compares
/// on data that turns out not to be HTTP at all.
pub const MAX_METHOD_LEN: usize = 20;

/// GET, PUT, ACL and the h2 preface PRI are the shortest registered methods.
pub const MIN_METHOD_LEN: usize = 3;

/// RFC 9110 9.1 lets a method be any token, but every registered one is
/// uppercase letters plus '-', and that narrow alphabet is what makes the
/// sniff safe on a port shared with TCP routes: an SSH banner ("SSH-2.0-Go ")
/// or a RESP frame ("*1\r\n") fails on its first digit or symbol instead of
/// being read as a request line. Lowercase methods are invalid on the wire
/// (nginx and haproxy reject them too), so they stay non-HTTP here.
#[inline(always)]
pub const fn is_method_byte(b: u8) -> bool {
    b.is_ascii_uppercase() || b == b'-'
}

/// Length of the method token opening `data` when a well-formed one is
/// followed by the request-line SP; `None` for anything else, including a
/// read so short the SP has not arrived yet. Borrows only, allocates never.
#[inline]
pub fn method_len(data: &[u8]) -> Option<usize> {
    if !data.first().is_some_and(|b| b.is_ascii_uppercase()) {
        return None;
    }
    for (i, &b) in data.iter().enumerate().take(MAX_METHOD_LEN + 1).skip(1) {
        if b == b' ' {
            return (i >= MIN_METHOD_LEN).then_some(i);
        }
        if !is_method_byte(b) {
            return None;
        }
    }
    None
}

/// Whether a configured route method is one portail could ever see on the
/// wire. Case-insensitive because the matcher is (`method: get` has always
/// matched GET); the alphabet is the wire alphabet above.
pub fn is_valid_method(method: &str) -> bool {
    let bytes = method.as_bytes();
    (MIN_METHOD_LEN..=MAX_METHOD_LEN).contains(&bytes.len())
        && bytes[0].is_ascii_alphabetic()
        && bytes
            .iter()
            .all(|&b| is_method_byte(b.to_ascii_uppercase()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const REGISTERED: &[&str] = &[
        "GET",
        "PUT",
        "ACL",
        "PRI",
        "HEAD",
        "POST",
        "COPY",
        "MOVE",
        "LOCK",
        "PATCH",
        "TRACE",
        "MKCOL",
        "DELETE",
        "UNLOCK",
        "REPORT",
        "SEARCH",
        "OPTIONS",
        "CONNECT",
        "PROPFIND",
        "PROPPATCH",
        "MKCALENDAR",
        "BASELINE-CONTROL",
        "UPDATEREDIRECTREF",
    ];

    #[test]
    fn registered_methods_are_tokens() {
        for m in REGISTERED {
            let line = format!("{m} / HTTP/1.1\r\n");
            assert_eq!(method_len(line.as_bytes()), Some(m.len()), "{m}");
            assert!(is_valid_method(m), "{m}");
        }
    }

    #[test]
    fn foreign_protocols_and_malformed_lines_are_not_methods() {
        let cases: &[&[u8]] = &[
            b"",
            b"GE",
            b"GE / HTTP/1.1",
            b"get / HTTP/1.1",
            b"-GET / HTTP/1.1",
            b"PROPFIND\r\n",
            b"PROPF",
            b"H2C / HTTP/1.1",
            b"AVERYLONGMETHODNAMETHATISNOT / HTTP/1.1",
            b"SSH-2.0-Go \r\n",
            b"*1\r\n$4\r\nPING\r\n",
            &[0x16, 0x03, 0x01, 0x02, 0x00, 0x01, 0x00, 0x01],
            b"\x00\x00\x00\x08\x04\xd2\x16\x2f",
        ];
        for c in cases {
            assert_eq!(method_len(c), None, "{:?}", String::from_utf8_lossy(c));
        }
    }

    #[test]
    fn token_at_the_length_bound() {
        let max = "M".repeat(MAX_METHOD_LEN);
        assert_eq!(
            method_len(format!("{max} /").as_bytes()),
            Some(MAX_METHOD_LEN)
        );
        let over = "M".repeat(MAX_METHOD_LEN + 1);
        assert_eq!(method_len(format!("{over} /").as_bytes()), None);
    }

    #[test]
    fn config_methods_are_case_insensitive_but_still_tokens() {
        assert!(is_valid_method("get"));
        assert!(is_valid_method("PropFind"));
        for bad in ["", "GE", "PRO PFIND", "GET1", "-GET", "GET "] {
            assert!(!is_valid_method(bad), "{bad:?}");
        }
    }
}
