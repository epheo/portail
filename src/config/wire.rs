//! What a route-filter string may contain, given that the data plane splices
//! it raw into HTTP/1 bytes: header lines, the request line, `Location`.
//! Standalone `validate()`, the Kubernetes converters and the route-table
//! build all check through here, so no config path reaches the wire unchecked.

use anyhow::{anyhow, Result};
use std::net::Ipv6Addr;

use super::types::{HeaderModifierConfig, HttpRouteFilter, HttpURLRewritePath};

/// RFC 9110 5.1: a field name is a non-empty token (5.6.2 tchar).
pub(crate) fn is_header_name(name: &[u8]) -> bool {
    !name.is_empty()
        && name
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(b))
}

/// First byte RFC 9110 5.5 forbids in a field value: every CTL except HTAB.
/// obs-text (0x80-0xFF) stays legal, which is what lets UTF-8 values through.
fn header_value_violation(value: &[u8]) -> Option<(usize, u8)> {
    value
        .iter()
        .position(|&b| (b < 0x20 && b != b'\t') || b == 0x7f)
        .map(|i| (i, value[i]))
}

fn check_header_name(name: &str) -> Result<()> {
    if is_header_name(name.as_bytes()) {
        Ok(())
    } else {
        Err(anyhow!("{:?} is not an RFC 9110 field name (token)", name))
    }
}

fn check_header_value(value: &str) -> Result<()> {
    // Never echo the value: modifiers often carry credentials, and this
    // message lands in route status and logs.
    match header_value_violation(value.as_bytes()) {
        None => Ok(()),
        Some((i, b)) => Err(anyhow!("control character 0x{:02x} at byte {}", b, i)),
    }
}

/// RFC 1123 host name, which also admits dotted IPv4. IPv6 must be
/// bracketed: bare, its colons collide with the port in Host and Location.
fn check_hostname(host: &str) -> Result<()> {
    let valid = match host.strip_prefix('[') {
        Some(rest) => rest
            .strip_suffix(']')
            .is_some_and(|v6| v6.parse::<Ipv6Addr>().is_ok()),
        None => is_dns_name(host),
    };
    if valid {
        Ok(())
    } else {
        Err(anyhow!(
            "{:?} is not a DNS name or IP literal ([v6] needs brackets)",
            host
        ))
    }
}

fn is_dns_name(host: &str) -> bool {
    host.len() <= 253
        && host.split('.').all(|label| {
            let l = label.as_bytes();
            (1..=63).contains(&l.len())
                && l.iter().all(|&b| b.is_ascii_alphanumeric() || b == b'-')
                && l[0] != b'-'
                && l[l.len() - 1] != b'-'
        })
}

fn check_path(path: &str) -> Result<()> {
    if !path.starts_with('/') {
        return Err(anyhow!("{:?} must start with '/'", path));
    }
    // SP would end the request-target mid-path; '#' is never part of one.
    match path
        .bytes()
        .position(|b| b <= b' ' || b == 0x7f || b == b'#')
    {
        None => Ok(()),
        Some(i) => Err(anyhow!(
            "{:?} has forbidden byte 0x{:02x} at {} (no SP, CTL or '#')",
            path,
            path.as_bytes()[i],
            i
        )),
    }
}

/// Any case (RFC 3986 3.1): the route table build lowercases it.
fn check_scheme(scheme: &str) -> Result<()> {
    if scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https") {
        Ok(())
    } else {
        Err(anyhow!("{:?} must be \"http\" or \"https\"", scheme))
    }
}

/// Headers that frame the message or the connection. The proxy frames the
/// forwarded message from the original one, so a modifier touching them,
/// remove included, desyncs the peer without a single CR/LF.
const REQUEST_FRAMING: &[&str] = &[
    "Content-Length",
    "Transfer-Encoding",
    "Connection",
    "Keep-Alive",
    "Proxy-Connection",
    "TE",
    "Trailer",
    "Upgrade",
    "Expect",
    "HTTP2-Settings",
];

const RESPONSE_FRAMING: &[&str] = &[
    "Content-Length",
    "Transfer-Encoding",
    "Connection",
    "Keep-Alive",
    "Proxy-Connection",
    "Trailer",
    "Upgrade",
];

#[derive(Clone, Copy, PartialEq)]
enum Side {
    Request,
    Response,
}

/// Every string of `filter` that the data plane writes to the wire. Errors
/// name the filter and the field, so route status points at the entry.
pub(crate) fn check_filter(filter: &HttpRouteFilter) -> Result<()> {
    match filter {
        HttpRouteFilter::RequestHeaderModifier { config } => {
            check_header_modifier(config, Side::Request)
                .map_err(|e| anyhow!("RequestHeaderModifier {}", e))
        }
        HttpRouteFilter::ResponseHeaderModifier { config } => {
            check_header_modifier(config, Side::Response)
                .map_err(|e| anyhow!("ResponseHeaderModifier {}", e))
        }
        HttpRouteFilter::RequestRedirect { config } => {
            if let Some(scheme) = &config.scheme {
                check_scheme(scheme).map_err(|e| anyhow!("RequestRedirect scheme: {}", e))?;
            }
            check_destination(
                "RequestRedirect",
                config.hostname.as_deref(),
                config.path.as_ref(),
            )
        }
        HttpRouteFilter::URLRewrite { config } => check_destination(
            "URLRewrite",
            config.hostname.as_deref(),
            config.path.as_ref(),
        ),
        // Resolved to a socket address; none of it reaches the wire.
        HttpRouteFilter::RequestMirror { .. } => Ok(()),
    }
}

fn check_header_modifier(m: &HeaderModifierConfig, side: Side) -> Result<()> {
    for (op, headers) in [("set", &m.set), ("add", &m.add)] {
        for (i, h) in headers.iter().enumerate() {
            check_modified_name(side, op, &h.name)
                .map_err(|e| anyhow!("{}[{}].name: {}", op, i, e))?;
            check_header_value(&h.value)
                .map_err(|e| anyhow!("{}[{}].value (header {:?}): {}", op, i, h.name, e))?;
        }
    }
    for (i, name) in m.remove.iter().enumerate() {
        check_modified_name(side, "remove", name).map_err(|e| anyhow!("remove[{}]: {}", i, e))?;
    }
    // The proxy replaces the Host line with the first matching set entry
    // and appends the rest.
    if side == Side::Request {
        let mut hosts = m.set.iter().enumerate().filter(|(_, h)| is_host(&h.name));
        if let (Some((first, _)), Some((i, h))) = (hosts.next(), hosts.next()) {
            return Err(anyhow!(
                "set[{}].name: {:?}: Host already set by set[{}], a second would send two Host lines",
                i,
                h.name,
                first
            ));
        }
    }
    Ok(())
}

fn check_modified_name(side: Side, op: &str, name: &str) -> Result<()> {
    check_header_name(name)?;
    let framing = match side {
        Side::Request => REQUEST_FRAMING,
        Side::Response => RESPONSE_FRAMING,
    };
    if framing.iter().any(|f| f.eq_ignore_ascii_case(name)) {
        return Err(anyhow!("{:?}: framing header is owned by the proxy", name));
    }
    if side == Side::Request && is_host(name) {
        let why = match op {
            "add" => "add would send two Host lines",
            "remove" => "an HTTP/1.1 request without Host is invalid",
            _ => return Ok(()),
        };
        return Err(anyhow!("{:?}: Host can only be set, {}", name, why));
    }
    Ok(())
}

fn is_host(name: &str) -> bool {
    name.eq_ignore_ascii_case("host")
}

/// Combinations within one rule that would put a malformed message on the
/// wire even though each filter is fine alone.
pub(crate) fn check_rule_filters(filters: &[HttpRouteFilter]) -> Result<()> {
    // URLRewrite rewrites the Host line in the same pass that applies the
    // rule's header modifier, so the modifier's Host is appended as a second.
    let rewrites_host = filters
        .iter()
        .any(|f| matches!(f, HttpRouteFilter::URLRewrite { config } if config.hostname.is_some()));
    let sets_host = filters.iter().any(|f| {
        matches!(f, HttpRouteFilter::RequestHeaderModifier { config }
            if config.set.iter().any(|h| is_host(&h.name)))
    });
    if rewrites_host && sets_host {
        return Err(anyhow!(
            "URLRewrite hostname and RequestHeaderModifier set \"Host\" would send two Host lines; keep one"
        ));
    }
    Ok(())
}

fn check_destination(
    filter: &str,
    hostname: Option<&str>,
    path: Option<&HttpURLRewritePath>,
) -> Result<()> {
    if let Some(host) = hostname {
        check_hostname(host).map_err(|e| anyhow!("{} hostname: {}", filter, e))?;
    }
    match path {
        None => Ok(()),
        Some(HttpURLRewritePath::ReplaceFullPath { value }) => {
            check_path(value).map_err(|e| anyhow!("{} path.replaceFullPath: {}", filter, e))
        }
        // Gateway API documents an empty replacement as "strip the prefix".
        Some(HttpURLRewritePath::ReplacePrefixMatch { value }) if value.is_empty() => Ok(()),
        Some(HttpURLRewritePath::ReplacePrefixMatch { value }) => {
            check_path(value).map_err(|e| anyhow!("{} path.replacePrefixMatch: {}", filter, e))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::types::{HttpHeader, RequestRedirectConfig, URLRewriteConfig};

    #[test]
    fn header_name_is_a_token() {
        for ok in ["X-Foo", "x", "Content-Type", "!#$%&'*+-.^_`|~09azAZ"] {
            assert!(check_header_name(ok).is_ok(), "{ok:?}");
        }
        for bad in [
            "",
            "X Foo",
            "X-Foo:",
            "X-Foo\r\nX-Injected",
            "X\0",
            "X\t",
            "(x)",
            "x\"",
            "caf\u{e9}",
        ] {
            assert!(check_header_name(bad).is_err(), "{bad:?}");
        }
        for b in 0u8..=0xff {
            let expected = b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b);
            assert_eq!(is_header_name(&[b]), expected, "byte 0x{b:02x}");
        }
    }

    #[test]
    fn header_value_rejects_every_ctl_but_htab() {
        for b in (0u8..0x20).chain([0x7f]) {
            let v = [b'a', b, b'b'];
            if b == b'\t' {
                assert_eq!(header_value_violation(&v), None);
            } else {
                assert_eq!(header_value_violation(&v), Some((1, b)), "0x{b:02x}");
            }
        }
        for v in ["ok\r\nX-Injected: 1", "ok\n", "ok\r", "nul\0"] {
            assert!(check_header_value(v).is_err(), "{v:?}");
        }
    }

    #[test]
    fn header_value_allows_htab_sp_vchar_and_obs_text() {
        for v in [
            "",
            "plain",
            "a\tb",
            " padded ",
            "q=0.9, */*",
            "caf\u{e9} \u{1f600}",
        ] {
            assert!(check_header_value(v).is_ok(), "{v:?}");
        }
        let obs_text: Vec<u8> = (0x80..=0xff).collect();
        assert_eq!(header_value_violation(&obs_text), None);
        let vchar: Vec<u8> = (0x21..=0x7e).collect();
        assert_eq!(header_value_violation(&vchar), None);
    }

    #[test]
    fn header_value_error_does_not_echo_the_value() {
        let err = check_header_value("Bearer s3cret\r\nX: y").unwrap_err();
        assert!(!err.to_string().contains("s3cret"), "{err}");
        assert!(err.to_string().contains("0x0d at byte 13"), "{err}");
    }

    #[test]
    fn hostname_is_dns_name_or_ip_literal() {
        for ok in [
            "example.com",
            "Example.COM",
            "a",
            "a-b.c0",
            "10.0.0.1",
            "[::1]",
            "[2001:db8::1]",
        ] {
            assert!(check_hostname(ok).is_ok(), "{ok:?}");
        }
        for bad in [
            "",
            "evil.com\r\nX-Injected: 1",
            "a b",
            "a\tb",
            "evil.com/path",
            "evil.com?q",
            "evil.com#frag",
            "user@evil.com",
            "host:8080",
            "::1",
            "[::1",
            "[not-v6]",
            "*.example.com",
            "-lead.com",
            "trail-.com",
            "a..b",
            "trailing.dot.",
            "under_score.com",
            "caf\u{e9}.com",
        ] {
            assert!(check_hostname(bad).is_err(), "{bad:?}");
        }
        assert!(check_hostname(&format!("{}.com", "a".repeat(63))).is_ok());
        assert!(check_hostname(&format!("{}.com", "a".repeat(64))).is_err());
        assert!(check_hostname(&["a"; 128].join(".")).is_err());
    }

    #[test]
    fn path_starts_with_slash_without_sp_ctl_or_fragment() {
        for ok in ["/", "/a/b", "/a?b=c", "/%20", "/caf\u{e9}", "/~user"] {
            assert!(check_path(ok).is_ok(), "{ok:?}");
        }
        for bad in [
            "",
            "a",
            "/a b",
            "/a\r\nX-Injected: 1",
            "/a\nb",
            "/a\tb",
            "/a\0",
            "/a\x7f",
            "/a#frag",
            "http://x/a",
        ] {
            assert!(check_path(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn scheme_is_http_or_https_in_any_case() {
        for ok in ["http", "https", "HTTPS", "Http"] {
            assert!(check_scheme(ok).is_ok(), "{ok:?}");
        }
        for bad in ["", "ftp", "javascript", "https\r\n", "HTTPS ", "httpss"] {
            assert!(check_scheme(bad).is_err(), "{bad:?}");
        }
    }

    fn header(name: &str, value: &str) -> HttpHeader {
        HttpHeader {
            name: name.to_string(),
            value: value.to_string(),
        }
    }

    fn modifier(
        add: Vec<HttpHeader>,
        set: Vec<HttpHeader>,
        remove: &[&str],
    ) -> HeaderModifierConfig {
        HeaderModifierConfig {
            add,
            set,
            remove: remove.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn filter_errors_name_filter_and_field() {
        let cases = [
            (
                HttpRouteFilter::RequestHeaderModifier {
                    config: modifier(
                        vec![],
                        vec![header("X-A", "ok"), header("X-B", "a\r\nb")],
                        &[],
                    ),
                },
                "RequestHeaderModifier set[1].value (header \"X-B\")",
            ),
            (
                HttpRouteFilter::ResponseHeaderModifier {
                    config: modifier(vec![header("X-A\r\nX-B", "v")], vec![], &[]),
                },
                "ResponseHeaderModifier add[0].name",
            ),
            (
                HttpRouteFilter::RequestHeaderModifier {
                    config: modifier(vec![], vec![], &["ok", "bad name"]),
                },
                "RequestHeaderModifier remove[1]",
            ),
            (
                HttpRouteFilter::RequestRedirect {
                    config: RequestRedirectConfig {
                        scheme: Some("gopher".to_string()),
                        hostname: None,
                        port: None,
                        path: None,
                        status_code: 302,
                    },
                },
                "RequestRedirect scheme",
            ),
            (
                HttpRouteFilter::RequestRedirect {
                    config: RequestRedirectConfig {
                        scheme: None,
                        hostname: Some("evil.com\r\nSet-Cookie: x".to_string()),
                        port: None,
                        path: None,
                        status_code: 302,
                    },
                },
                "RequestRedirect hostname",
            ),
            (
                HttpRouteFilter::URLRewrite {
                    config: URLRewriteConfig {
                        hostname: None,
                        path: Some(HttpURLRewritePath::ReplaceFullPath {
                            value: "/a HTTP/1.1\r\n\r\nGET /smuggled".to_string(),
                        }),
                    },
                },
                "URLRewrite path.replaceFullPath",
            ),
            (
                HttpRouteFilter::URLRewrite {
                    config: URLRewriteConfig {
                        hostname: None,
                        path: Some(HttpURLRewritePath::ReplacePrefixMatch {
                            value: "v2".to_string(),
                        }),
                    },
                },
                "URLRewrite path.replacePrefixMatch",
            ),
        ];
        for (filter, expected) in cases {
            let err = check_filter(&filter).unwrap_err().to_string();
            assert!(err.starts_with(expected), "{err}");
        }
    }

    #[test]
    fn filter_accepts_valid_values_and_empty_prefix_replacement() {
        let filters = [
            HttpRouteFilter::RequestHeaderModifier {
                config: modifier(
                    vec![header("X-Added", "caf\u{e9}\tok")],
                    vec![header("X-Set", "")],
                    &["X-Remove"],
                ),
            },
            HttpRouteFilter::RequestRedirect {
                config: RequestRedirectConfig {
                    scheme: Some("https".to_string()),
                    hostname: Some("[::1]".to_string()),
                    port: Some(8443),
                    path: Some(HttpURLRewritePath::ReplacePrefixMatch {
                        value: String::new(),
                    }),
                    status_code: 301,
                },
            },
            HttpRouteFilter::URLRewrite {
                config: URLRewriteConfig {
                    hostname: Some("internal.example.com".to_string()),
                    path: Some(HttpURLRewritePath::ReplaceFullPath {
                        value: "/v2?x=1".to_string(),
                    }),
                },
            },
        ];
        for f in &filters {
            assert!(check_filter(f).is_ok(), "{f:?}");
        }
    }

    fn modifier_errors(
        names: &[&str],
        wrap: fn(HeaderModifierConfig) -> HttpRouteFilter,
    ) -> Vec<(String, String)> {
        let mut out = Vec::new();
        for name in names {
            for spelling in [name.to_string(), name.to_ascii_lowercase()] {
                for (op, m) in [
                    (
                        "set[0].name",
                        modifier(vec![], vec![header(&spelling, "v")], &[]),
                    ),
                    (
                        "add[0].name",
                        modifier(vec![header(&spelling, "v")], vec![], &[]),
                    ),
                    ("remove[0]", modifier(vec![], vec![], &[&spelling])),
                ] {
                    let got =
                        check_filter(&wrap(m)).map_or_else(|e| e.to_string(), |_| "ok".into());
                    out.push((format!("{op}: {spelling:?}"), got));
                }
            }
        }
        out
    }

    #[test]
    fn request_modifier_rejects_framing_headers() {
        let names = [
            "Content-Length",
            "Transfer-Encoding",
            "Connection",
            "Keep-Alive",
            "Proxy-Connection",
            "TE",
            "Trailer",
            "Upgrade",
            "Expect",
            "HTTP2-Settings",
        ];
        let wrap = |config| HttpRouteFilter::RequestHeaderModifier { config };
        for (field, got) in modifier_errors(&names, wrap) {
            assert_eq!(
                got,
                format!("RequestHeaderModifier {field}: framing header is owned by the proxy")
            );
        }

        let host = |m| check_filter(&wrap(m)).map_err(|e| e.to_string());
        assert!(host(modifier(vec![], vec![header("Host", "internal")], &[])).is_ok());
        let add = host(modifier(vec![header("host", "x")], vec![], &[])).unwrap_err();
        assert!(
            add.starts_with("RequestHeaderModifier add[0].name: \"host\": Host can only be set")
        );
        let remove = host(modifier(vec![], vec![], &["HOST"])).unwrap_err();
        assert!(
            remove.starts_with("RequestHeaderModifier remove[0]: \"HOST\": Host can only be set")
        );
        let twice = host(modifier(
            vec![],
            vec![header("Host", "a"), header("X-A", "b"), header("host", "c")],
            &[],
        ))
        .unwrap_err();
        assert!(
            twice.starts_with(
                "RequestHeaderModifier set[2].name: \"host\": Host already set by set[0]"
            ),
            "{twice}"
        );
    }

    #[test]
    fn response_modifier_rejects_framing_headers() {
        let names = [
            "Content-Length",
            "Transfer-Encoding",
            "Connection",
            "Keep-Alive",
            "Proxy-Connection",
            "Trailer",
            "Upgrade",
        ];
        let wrap = |config| HttpRouteFilter::ResponseHeaderModifier { config };
        for (field, got) in modifier_errors(&names, wrap) {
            assert_eq!(
                got,
                format!("ResponseHeaderModifier {field}: framing header is owned by the proxy")
            );
        }
        // Host rules are request-side only.
        let m = modifier(vec![header("Host", "x")], vec![], &["Host"]);
        assert!(check_filter(&wrap(m)).is_ok());
    }

    #[test]
    fn host_rewrite_and_host_set_in_one_rule_conflict() {
        let rewrite = |hostname: Option<&str>| HttpRouteFilter::URLRewrite {
            config: URLRewriteConfig {
                hostname: hostname.map(str::to_string),
                path: Some(HttpURLRewritePath::ReplaceFullPath {
                    value: "/".to_string(),
                }),
            },
        };
        let set = |name: &str| HttpRouteFilter::RequestHeaderModifier {
            config: modifier(vec![], vec![header(name, "internal")], &[]),
        };
        let err = check_rule_filters(&[set("HOST"), rewrite(Some("a.example.com"))]).unwrap_err();
        assert!(err.to_string().contains("two Host lines"), "{err}");
        assert!(check_rule_filters(&[rewrite(None), set("Host")]).is_ok());
        assert!(check_rule_filters(&[rewrite(Some("a.example.com")), set("X-A")]).is_ok());
    }
}
