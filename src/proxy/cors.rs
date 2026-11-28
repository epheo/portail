//! The HTTPRoute CORS filter: which origins a rule shares its responses
//! with, and the headers that say so.
//!
//! An allowed origin is always echoed, never answered with `*`: that is
//! right with and without credentials, and one code path. So every
//! response of a rule with this filter varies by `Origin`.

use std::fmt::Write;

use crate::config::CorsConfig;
use crate::routing::find_header_value;

/// A rule's CORS policy, checked and rendered as far as no request is
/// needed for it.
#[derive(Debug)]
pub struct Cors {
    origins: Vec<Allowed>,
    methods: Listed,
    headers: Listed,
    /// The lines every allowed response carries after its origin:
    /// credentials and exposed headers.
    shared: Box<str>,
    max_age: u32,
}

/// What `Access-Control-Max-Age` says when the filter does not (Gateway
/// API's default).
const DEFAULT_MAX_AGE: u32 = 5;

#[derive(Debug)]
enum Allowed {
    Any,
    Exactly(Site),
    /// Any host ending in `host`, the `*` of the pattern being whatever
    /// comes before: labels, dots and all.
    Under(Site),
}

/// An origin, compared as browsers serialize it: scheme and host without
/// case, the scheme's own port when none is written.
#[derive(Debug, PartialEq, Eq)]
struct Site {
    https: bool,
    host: Box<str>,
    port: u16,
}

/// An allow-list a preflight asks about.
#[derive(Debug)]
enum Listed {
    Unset,
    /// `*`: what the preflight asks for is echoed, which a wildcard may
    /// not replace once credentials are allowed.
    Any,
    These(Box<str>),
}

/// What the gateway does with a request on a rule with this filter.
#[derive(Debug, PartialEq, Eq)]
pub enum Answer {
    /// Forward it; the response headers are written.
    Forward,
    /// A preflight, answered here: allowed, or not.
    Preflight(bool),
}

impl Cors {
    pub fn new(config: &CorsConfig) -> Result<Self, String> {
        let origins = config
            .allow_origins
            .iter()
            .map(|pattern| allowed(pattern))
            .collect::<Result<_, _>>()?;
        let mut shared = String::new();
        if config.allow_credentials {
            shared.push_str("Access-Control-Allow-Credentials: true\r\n");
        }
        // With credentials a wildcard may not be sent, and the names it
        // stands for are the backend's to know: nothing more is exposed.
        match listed("exposeHeaders", &config.expose_headers)? {
            Listed::These(names) => {
                let _ = write!(shared, "Access-Control-Expose-Headers: {names}\r\n");
            }
            Listed::Any if !config.allow_credentials => {
                shared.push_str("Access-Control-Expose-Headers: *\r\n");
            }
            Listed::Any | Listed::Unset => {}
        }
        Ok(Self {
            origins,
            methods: listed("allowMethods", &config.allow_methods)?,
            headers: listed("allowHeaders", &config.allow_headers)?,
            shared: shared.into(),
            max_age: config.max_age.unwrap_or(DEFAULT_MAX_AGE),
        })
    }

    /// Judge a request by its method and header block, and write into
    /// `lines` the header lines its response carries, whoever writes that
    /// response: the backend, or the gateway answering in its place.
    pub fn answer(&self, method: &str, header_data: &[u8], lines: &mut Vec<u8>) -> Answer {
        lines.extend_from_slice(b"Vary: Origin\r\n");
        let origin = find_header_value(header_data, "origin");
        let asked = find_header_value(header_data, "access-control-request-method");
        let preflight = match (method, origin, asked) {
            ("OPTIONS", Some(_), Some(asked)) => Some(asked),
            _ => None,
        };
        let Some(origin) = origin.filter(|origin| self.allows(origin)) else {
            return match preflight {
                Some(_) => Answer::Preflight(false),
                None => Answer::Forward,
            };
        };
        lines.extend_from_slice(b"Access-Control-Allow-Origin: ");
        lines.extend_from_slice(origin.as_bytes());
        lines.extend_from_slice(b"\r\n");
        lines.extend_from_slice(self.shared.as_bytes());
        let Some(asked) = preflight else {
            return Answer::Forward;
        };
        let asked_headers = find_header_value(header_data, "access-control-request-headers");
        self.methods
            .line("Access-Control-Allow-Methods", Some(asked), lines);
        self.headers
            .line("Access-Control-Allow-Headers", asked_headers, lines);
        let _ = write!(
            LineWriter(lines),
            "Access-Control-Max-Age: {}\r\n",
            self.max_age
        );
        Answer::Preflight(true)
    }

    fn allows(&self, origin: &str) -> bool {
        // Parsed whatever the list says: an origin is echoed, so it must
        // be one.
        let Some(site) = site(origin) else {
            return false;
        };
        self.origins.iter().any(|allowed| match allowed {
            Allowed::Any => true,
            Allowed::Exactly(it) => *it == site,
            Allowed::Under(it) => {
                it.https == site.https
                    && it.port == site.port
                    && site.host.len() > it.host.len()
                    && site.host.ends_with(&*it.host)
            }
        })
    }
}

impl Listed {
    /// The allow header for a preflight that asks for `asked`.
    fn line(&self, name: &str, asked: Option<&str>, lines: &mut Vec<u8>) {
        let value = match self {
            Listed::Unset => return,
            Listed::These(these) => these,
            // Echoed only if it is a list of names: it becomes a header.
            Listed::Any => match asked.filter(|asked| is_name_list(asked)) {
                Some(asked) => asked,
                None => return,
            },
        };
        lines.extend_from_slice(name.as_bytes());
        lines.extend_from_slice(b": ");
        lines.extend_from_slice(value.as_bytes());
        lines.extend_from_slice(b"\r\n");
    }
}

struct LineWriter<'a>(&'a mut Vec<u8>);

impl Write for LineWriter<'_> {
    fn write_str(&mut self, s: &str) -> std::fmt::Result {
        self.0.extend_from_slice(s.as_bytes());
        Ok(())
    }
}

/// `lines` set on a response head: any of the same headers the backend
/// sent is dropped, the filter's word being the rule's on CORS. `head`
/// ends with its blank line and still does after.
pub(crate) fn set_on(head: &mut Vec<u8>, lines: &[u8]) {
    const OWNED: [&[u8]; 3] = [
        b"access-control-allow-origin",
        b"access-control-allow-credentials",
        b"access-control-expose-headers",
    ];
    // Past the status line; a head is never without one.
    let mut at = memchr::memchr(b'\n', head).map_or(head.len(), |nl| nl + 1);
    while at < head.len() {
        let end = memchr::memchr(b'\n', &head[at..]).map_or(head.len(), |nl| at + nl + 1);
        let line = &head[at..end];
        let name = memchr::memchr(b':', line).map_or(&line[..0], |colon| &line[..colon]);
        if OWNED.iter().any(|owned| name.eq_ignore_ascii_case(owned)) {
            head.drain(at..end);
        } else {
            at = end;
        }
    }
    // The blank line `sanitize_response_headers` and the header modifier
    // both end a head with.
    debug_assert!(head.ends_with(b"\r\n\r\n"));
    let blank = head.len().saturating_sub(2);
    head.splice(blank..blank, lines.iter().copied());
}

fn is_token_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b)
}

fn is_token(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(is_token_byte)
}

/// Names separated by commas, with the blanks browsers put around them.
fn is_name_list(s: &str) -> bool {
    !s.is_empty()
        && s.bytes()
            .all(|b| is_token_byte(b) || b == b',' || b == b' ')
}

fn listed(field: &str, names: &[String]) -> Result<Listed, String> {
    if names.is_empty() {
        return Ok(Listed::Unset);
    }
    if names.iter().any(|name| name == "*") {
        return Ok(Listed::Any);
    }
    match names.iter().find(|name| !is_token(name)) {
        Some(bad) => Err(format!("CORS {field}: {bad:?} is no name")),
        None => Ok(Listed::These(names.join(", ").into())),
    }
}

fn allowed(pattern: &str) -> Result<Allowed, String> {
    if pattern == "*" {
        return Ok(Allowed::Any);
    }
    let parsed = match pattern.split_once("://*") {
        // Read with a label where the wildcard stands, taken off again.
        Some((scheme, under)) => site(&format!("{scheme}://a{under}")).map(|mut under| {
            under.host = under.host[1..].into();
            Allowed::Under(under)
        }),
        None => site(pattern).map(Allowed::Exactly),
    };
    parsed.ok_or_else(|| {
        format!(
            "CORS allowOrigins: {pattern:?} is neither `*` nor `<scheme>://<host>[:<port>]`, \
             the host at most starting with a `*`"
        )
    })
}

/// `<scheme>://<host>[:<port>]` and nothing else, as an origin is written.
fn site(origin: &str) -> Option<Site> {
    let (scheme, rest) = origin.split_once("://")?;
    let https = match scheme {
        s if s.eq_ignore_ascii_case("https") => true,
        s if s.eq_ignore_ascii_case("http") => false,
        _ => return None,
    };
    let (host, port) = match rest.strip_prefix('[') {
        // An IPv6 literal keeps its brackets: they are part of how it is
        // written, and of what is echoed.
        Some(v6) => {
            let (address, after) = v6.split_once(']')?;
            let literal = !address.is_empty()
                && address
                    .bytes()
                    .all(|b| b.is_ascii_hexdigit() || b == b':' || b == b'.');
            let port = match after {
                "" => None,
                after => Some(after.strip_prefix(':')?),
            };
            (literal.then_some(&rest[..address.len() + 2])?, port)
        }
        None => {
            let (host, port) = match rest.split_once(':') {
                Some((host, port)) => (host, Some(port)),
                None => (rest, None),
            };
            let named = !host.is_empty()
                && host
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-._".contains(&b));
            (named.then_some(host)?, port)
        }
    };
    let port = match port {
        None => {
            if https {
                443
            } else {
                80
            }
        }
        Some(port) if !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()) => {
            port.parse().ok().filter(|port| *port != 0)?
        }
        Some(_) => return None,
    };
    Some(Site {
        https,
        host: host.to_ascii_lowercase().into(),
        port,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cors(json: serde_json::Value) -> Cors {
        Cors::new(&serde_json::from_value(json).unwrap()).unwrap()
    }

    fn answer(cors: &Cors, method: &str, headers: &str) -> (Answer, String) {
        let mut lines = Vec::new();
        let answer = cors.answer(method, headers.as_bytes(), &mut lines);
        (answer, String::from_utf8(lines).unwrap())
    }

    fn origin_of(lines: &str) -> Option<&str> {
        lines
            .split("\r\n")
            .find_map(|line| line.strip_prefix("Access-Control-Allow-Origin: "))
    }

    /// The origins Gateway API's conformance test sends, and where a
    /// pattern must stop.
    #[test]
    fn an_origin_is_allowed_exactly_or_under_a_wildcard() {
        let it = cors(serde_json::json!({"allowOrigins": [
            "https://www.foo.com", "https://*.bar.com", "http://*:8080", "https://[::1]:8443"
        ]}));
        for (origin, allowed) in [
            ("https://www.foo.com", true),
            ("HTTPS://WWW.FOO.COM", true),
            ("https://www.foo.com:443", true),
            ("https://www.bar.com", true),
            ("https://xpto.www.bar.com", true),
            ("http://anything.at.all:8080", true),
            ("https://[::1]:8443", true),
            ("https://foobar.com", false),
            ("http://www.foo.com", false),
            ("https://www.foo.com:8443", false),
            ("https://bar.com", false),
            ("https://.bar.com", false),
            ("https://evilbar.com", false),
            ("https://www.bar.com:8443", false),
            ("https://www.bar.com.evil.test", false),
            ("http://anything.at.all", false),
            ("https://[::1]", false),
            ("null", false),
            ("https://www.foo.com/", false),
            ("https://www.foo.com, https://x", false),
            ("ftp://www.foo.com", false),
        ] {
            let (_, lines) = answer(&it, "GET", &format!("Origin: {origin}\r\n"));
            assert_eq!(origin_of(&lines).is_some(), allowed, "{origin}");
        }
    }

    /// An allowed origin comes back as it was sent, `*` never: with
    /// credentials a wildcard is refused by browsers.
    #[test]
    fn an_allowed_origin_is_echoed_and_the_response_varies_by_it() {
        let it = cors(serde_json::json!({
            "allowOrigins": ["*"], "allowCredentials": true, "exposeHeaders": ["x-a", "x-b"]
        }));
        let (what, lines) = answer(&it, "GET", "Origin: https://Foobar.com:12345\r\n");
        assert_eq!(what, Answer::Forward);
        assert_eq!(
            lines,
            "Vary: Origin\r\n\
             Access-Control-Allow-Origin: https://Foobar.com:12345\r\n\
             Access-Control-Allow-Credentials: true\r\n\
             Access-Control-Expose-Headers: x-a, x-b\r\n"
        );

        for (headers, why) in [
            ("", "no Origin"),
            ("Origin: null\r\n", "no site"),
            ("Origin: https://a.test\tx\r\n", "a control byte"),
        ] {
            let (what, lines) = answer(&it, "GET", headers);
            assert_eq!(
                (what, &*lines),
                (Answer::Forward, "Vary: Origin\r\n"),
                "{why}"
            );
        }
    }

    #[test]
    fn a_preflight_is_answered_with_the_lists_or_with_what_it_asks() {
        let listed = cors(serde_json::json!({
            "allowOrigins": ["https://www.foo.com"],
            "allowMethods": ["GET", "OPTIONS"],
            "allowHeaders": ["x-header-1", "x-header-2"],
            "maxAge": 3600
        }));
        let preflight = "Origin: https://www.foo.com\r\n\
                         Access-Control-Request-Method: PUT\r\n\
                         Access-Control-Request-Headers: x-asked, x-too\r\n";
        let (what, lines) = answer(&listed, "OPTIONS", preflight);
        assert_eq!(what, Answer::Preflight(true));
        assert_eq!(
            lines,
            "Vary: Origin\r\n\
             Access-Control-Allow-Origin: https://www.foo.com\r\n\
             Access-Control-Allow-Methods: GET, OPTIONS\r\n\
             Access-Control-Allow-Headers: x-header-1, x-header-2\r\n\
             Access-Control-Max-Age: 3600\r\n"
        );

        let any = cors(serde_json::json!({
            "allowOrigins": ["*"], "allowMethods": ["*"], "allowHeaders": ["*"],
            "exposeHeaders": ["*"], "allowCredentials": true
        }));
        let (what, lines) = answer(&any, "OPTIONS", preflight);
        assert_eq!(what, Answer::Preflight(true));
        assert_eq!(
            lines,
            "Vary: Origin\r\n\
             Access-Control-Allow-Origin: https://www.foo.com\r\n\
             Access-Control-Allow-Credentials: true\r\n\
             Access-Control-Allow-Methods: PUT\r\n\
             Access-Control-Allow-Headers: x-asked, x-too\r\n\
             Access-Control-Max-Age: 5\r\n"
        );
        // Nothing asked, nothing to echo; and what is no list of names is
        // not echoed.
        for asked in ["", "Access-Control-Request-Headers: x-a\tinjected: y\r\n"] {
            let headers = format!(
                "Origin: https://www.foo.com\r\nAccess-Control-Request-Method: PUT\r\n{asked}"
            );
            let (_, lines) = answer(&any, "OPTIONS", &headers);
            assert!(!lines.contains("Access-Control-Allow-Headers"), "{lines}");
        }

        let (what, lines) = answer(
            &listed,
            "OPTIONS",
            "Origin: https://foobar.com\r\nAccess-Control-Request-Method: GET\r\n",
        );
        assert_eq!(
            (what, &*lines),
            (Answer::Preflight(false), "Vary: Origin\r\n")
        );
    }

    /// OPTIONS alone is a request like another: a preflight names an
    /// origin and the method it asks for.
    #[test]
    fn options_without_a_requested_method_is_forwarded() {
        let it = cors(serde_json::json!({"allowOrigins": ["*"], "allowMethods": ["PUT"]}));
        for headers in [
            "Origin: https://a.test\r\n",
            "Access-Control-Request-Method: PUT\r\n",
        ] {
            assert_eq!(answer(&it, "OPTIONS", headers).0, Answer::Forward);
        }
        let asked = "Origin: https://a.test\r\nAccess-Control-Request-Method: PUT\r\n";
        assert_eq!(answer(&it, "GET", asked).0, Answer::Forward);
    }

    /// Without credentials a wildcard list of exposed headers is sent as
    /// one; with them nothing is, the wildcard being refused then.
    #[test]
    fn a_wildcard_of_exposed_headers_is_sent_only_without_credentials() {
        let lines = |credentials: bool| {
            let it = cors(serde_json::json!({
                "allowOrigins": ["*"], "exposeHeaders": ["*"], "allowCredentials": credentials
            }));
            answer(&it, "GET", "Origin: https://a.test\r\n").1
        };
        assert!(lines(false).contains("Access-Control-Expose-Headers: *\r\n"));
        assert!(!lines(true).contains("Access-Control-Expose-Headers"));
    }

    #[test]
    fn what_cannot_be_a_header_is_refused_when_the_rule_is_built() {
        for (config, why) in [
            (
                serde_json::json!({"allowOrigins": ["www.foo.com"]}),
                "allowOrigins",
            ),
            (
                serde_json::json!({"allowOrigins": ["https://a.*.test"]}),
                "allowOrigins",
            ),
            (
                serde_json::json!({"allowOrigins": ["https://a.test/path"]}),
                "allowOrigins",
            ),
            (
                serde_json::json!({"allowOrigins": ["https://a.test\r\nX: y"]}),
                "allowOrigins",
            ),
            (
                serde_json::json!({"allowMethods": ["GET\r\nX: y"]}),
                "allowMethods",
            ),
            (serde_json::json!({"allowHeaders": ["x a"]}), "allowHeaders"),
            (
                serde_json::json!({"exposeHeaders": ["x,a"]}),
                "exposeHeaders",
            ),
        ] {
            let refused = Cors::new(&serde_json::from_value(config).unwrap()).unwrap_err();
            assert!(refused.contains(why), "{refused}");
        }
    }

    /// The backend's own CORS headers go, whatever the filter decided,
    /// and the rest of the head stays as it was.
    #[test]
    fn the_filters_lines_replace_the_backends() {
        let mut head = b"HTTP/1.1 200 OK\r\n\
              Content-Length: 2\r\n\
              access-control-allow-origin: *\r\n\
              X-Kept: 1\r\n\
              Access-Control-Expose-Headers: x\r\n\
              Access-Control-Allow-Credentials: true\r\n\
              \r\n"
            .to_vec();
        set_on(&mut head, b"Vary: Origin\r\n");
        assert_eq!(
            String::from_utf8(head).unwrap(),
            "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nX-Kept: 1\r\nVary: Origin\r\n\r\n"
        );

        let mut head = b"HTTP/1.1 204 No Content\r\n\r\n".to_vec();
        set_on(
            &mut head,
            b"Vary: Origin\r\nAccess-Control-Allow-Origin: https://a.test\r\n",
        );
        assert_eq!(
            String::from_utf8(head).unwrap(),
            "HTTP/1.1 204 No Content\r\nVary: Origin\r\n\
             Access-Control-Allow-Origin: https://a.test\r\n\r\n"
        );
    }

    use crate::proxy::testkit::{
        assert_never_connected, backend, exchange, idle_backend, refusing_backend, response, serve,
    };
    use crate::routing::RouteTable;
    use tokio::io::AsyncWriteExt;

    const POLICY: &str = r#"{"allowOrigins": ["https://app.test"], "allowMethods": ["PUT"],
                             "allowCredentials": true}"#;
    const PREFLIGHT: &str = "OPTIONS /x HTTP/1.1\r\nHost: a.test\r\nOrigin: https://app.test\r\n\
                               Access-Control-Request-Method: PUT\r\n\r\n";
    const GET: &str = "GET /x HTTP/1.1\r\nHost: a.test\r\nOrigin: https://app.test\r\n\r\n";

    /// a.test under `POLICY`, proxying to `backend_ref`; b.test without a
    /// filter, to the same. Built as a snapshot or a file gives it.
    fn routes(backend_ref: serde_json::Value) -> RouteTable {
        let policy: serde_json::Value = serde_json::from_str(POLICY).unwrap();
        let config: crate::config::PortailConfig = serde_json::from_value(serde_json::json!({
            "gateway": {"name": "g", "listeners": [{"name": "http", "protocol": "HTTP", "port": 8080}]},
            "httpRoutes": [
                {"parentRefs": [{"name": "g", "sectionName": "http"}], "hostnames": ["a.test"],
                 "rules": [{"filters": [{"type": "CORS", "cors": policy}], "backendRefs": [backend_ref]}]},
                {"parentRefs": [{"name": "g", "sectionName": "http"}], "hostnames": ["b.test"],
                 "rules": [{"backendRefs": [backend_ref]}]}
            ]
        }))
        .unwrap();
        config.validate().unwrap();
        config.to_route_table_strict().unwrap()
    }

    fn to(backend: std::net::SocketAddr) -> serde_json::Value {
        serde_json::json!({"name": "127.0.0.1", "port": backend.port()})
    }

    fn header<'a>(response: &'a str, name: &str) -> Vec<&'a str> {
        response
            .split("\r\n")
            .filter_map(|line| line.split_once(": "))
            .filter(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, value)| value)
            .collect()
    }

    /// The gateway answers a preflight itself: no backend is asked, and
    /// the connection is kept for the request the browser sends next.
    #[tokio::test]
    async fn a_preflight_is_answered_here_and_the_connection_kept() {
        let silent = idle_backend().await;
        let (mut client, _engine) = serve(routes(to(silent.local_addr().unwrap())));

        for _ in 0..2 {
            client.write_all(PREFLIGHT.as_bytes()).await.unwrap();
            let answer = response(&mut client).await;
            assert!(
                answer.starts_with("HTTP/1.1 204 No Content\r\n"),
                "{answer}"
            );
            assert_eq!(
                header(&answer, "access-control-allow-origin"),
                ["https://app.test"]
            );
            assert_eq!(header(&answer, "access-control-allow-methods"), ["PUT"]);
            assert_eq!(
                header(&answer, "access-control-allow-credentials"),
                ["true"]
            );
            assert_eq!(header(&answer, "access-control-max-age"), ["5"]);
            assert_eq!(header(&answer, "vary"), ["Origin"]);
            assert!(header(&answer, "content-length").is_empty(), "{answer}");
        }
        assert_never_connected(&silent).await;

        let refused = PREFLIGHT.replace("app.test", "bad.test");
        let answer = exchange(&mut client, refused.as_bytes()).await;
        assert!(answer.starts_with("HTTP/1.1 403 Forbidden\r\n"), "{answer}");
        assert!(
            header(&answer, "access-control-allow-origin").is_empty(),
            "{answer}"
        );
    }

    /// On a response the filter's headers stand in for the backend's own,
    /// and a request from elsewhere gets none.
    #[tokio::test]
    async fn a_response_is_shared_with_the_origins_the_filter_allows() {
        let (addr, _seen) = backend(
            b"HTTP/1.1 200 OK\r\nAccess-Control-Allow-Origin: *\r\nContent-Length: 2\r\n\r\nok",
        )
        .await;
        for (origin, shared) in [
            ("Origin: https://app.test\r\n", vec!["https://app.test"]),
            ("Origin: https://bad.test\r\n", vec![]),
            ("", vec![]),
        ] {
            let (mut client, _engine) = serve(routes(to(addr)));
            let request = format!("GET /x HTTP/1.1\r\nHost: a.test\r\n{origin}\r\n");
            let answer = exchange(&mut client, request.as_bytes()).await;
            assert!(answer.starts_with("HTTP/1.1 200 OK\r\n"), "{answer}");
            assert_eq!(
                header(&answer, "access-control-allow-origin"),
                shared,
                "{origin}"
            );
            assert_eq!(header(&answer, "vary"), ["Origin"], "{origin}");
            assert!(answer.ends_with("\r\n\r\nok"), "{answer}");
        }
    }

    /// An answer the gateway writes in the backend's place says so to the
    /// page that asked: without the headers a browser reports a CORS
    /// failure, not the 500 or the 502.
    #[tokio::test]
    async fn a_gateway_error_is_shared_as_a_response_is() {
        let (_held, refusing) = refusing_backend();
        let invalid = serde_json::json!({"name": "127.0.0.1", "port": 9, "invalid": true});
        for (backend_ref, status) in [(to(refusing), "502 Bad Gateway"), (invalid, "500 ")] {
            let (mut client, _engine) = serve(routes(backend_ref.clone()));
            let answer = exchange(&mut client, GET.as_bytes()).await;
            assert!(
                answer.starts_with(&format!("HTTP/1.1 {status}")),
                "{answer}"
            );
            assert_eq!(
                header(&answer, "access-control-allow-origin"),
                ["https://app.test"]
            );
            assert_eq!(
                header(&answer, "access-control-allow-credentials"),
                ["true"]
            );

            // And a preflight is answered whatever the backends are.
            let (mut client, _engine) = serve(routes(backend_ref));
            let answer = exchange(&mut client, PREFLIGHT.as_bytes()).await;
            assert!(answer.starts_with("HTTP/1.1 204 "), "{answer}");
        }
    }

    /// The lines are a request's: the next one on the connection, under a
    /// rule without the filter, carries none of them.
    #[tokio::test]
    async fn a_requests_lines_do_not_outlive_it() {
        let (addr, _seen) = backend(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok").await;
        let (mut client, _engine) = serve(routes(to(addr)));

        client.write_all(PREFLIGHT.as_bytes()).await.unwrap();
        let answer = response(&mut client).await;
        assert_eq!(
            header(&answer, "access-control-allow-origin"),
            ["https://app.test"]
        );

        let elsewhere = GET.replace("Host: a.test", "Host: b.test");
        let answer = exchange(&mut client, elsewhere.as_bytes()).await;
        assert!(answer.starts_with("HTTP/1.1 200 OK\r\n"), "{answer}");
        assert!(
            !answer.to_ascii_lowercase().contains("access-control"),
            "{answer}"
        );
        assert!(header(&answer, "vary").is_empty(), "{answer}");
    }

    #[test]
    fn a_rule_takes_one_cors_filter_and_a_backend_ref_none() {
        let build = |rule: serde_json::Value| {
            let config: crate::config::PortailConfig = serde_json::from_value(serde_json::json!({
                "gateway": {"name": "g", "listeners": [{"name": "http", "protocol": "HTTP", "port": 8080}]},
                "httpRoutes": [{"parentRefs": [{"name": "g", "sectionName": "http"}],
                                "hostnames": ["a.test"], "rules": [rule]}]
            }))
            .unwrap();
            config
                .to_route_table_strict()
                .map(drop)
                .map_err(|e| format!("{e:#}"))
        };
        let filter = |origins: &str| serde_json::json!({"type": "CORS", "cors": {"allowOrigins": [origins]}});
        let backend = serde_json::json!({"name": "127.0.0.1", "port": 9});
        let mut filtered = backend.clone();
        filtered["filters"] = serde_json::json!([filter("*")]);

        build(serde_json::json!({"filters": [filter("*")], "backendRefs": [backend]})).unwrap();
        for (rule, why) in [
            (
                serde_json::json!({"filters": [filter("*"), filter("*")], "backendRefs": [backend]}),
                "repeated",
            ),
            (
                serde_json::json!({"filters": [filter("app.test")], "backendRefs": [backend]}),
                "allowOrigins",
            ),
            (
                serde_json::json!({"backendRefs": [filtered]}),
                "backend ref",
            ),
        ] {
            let refused = build(rule).unwrap_err();
            assert!(refused.contains(why), "{refused}");
        }
    }
}
