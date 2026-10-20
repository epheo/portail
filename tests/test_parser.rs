use portail::proxy::http_parser::{extract_routing_info, HttpRequestInfo};

fn rejected(req: &[u8]) -> bool {
    match extract_routing_info(req) {
        Ok(info) => info.header_violation,
        Err(_) => true,
    }
}

#[track_caller]
fn assert_rejected(req: &[u8]) {
    assert!(
        rejected(req),
        "accepted: {:?}",
        String::from_utf8_lossy(req)
    );
}

#[track_caller]
fn accepted(req: &[u8]) -> HttpRequestInfo<'_> {
    match extract_routing_info(req) {
        Ok(info) if !info.header_violation => info,
        Ok(_) => panic!("violation: {:?}", String::from_utf8_lossy(req)),
        Err(e) => panic!("{e}: {:?}", String::from_utf8_lossy(req)),
    }
}

/// `line` plus CRLF, then `headers` (each line CRLF-terminated), then the
/// blank line.
fn head(line: &str, headers: &str) -> Vec<u8> {
    format!("{line}\r\n{headers}\r\n").into_bytes()
}

/// A request around one extra raw header line.
fn with_header(line: &[u8]) -> Vec<u8> {
    let mut req = b"POST / HTTP/1.1\r\nHost: h\r\n".to_vec();
    req.extend_from_slice(line);
    req.extend_from_slice(b"\r\n\r\n");
    req
}

fn stripped<'a>(req: &'a [u8], info: &HttpRequestInfo<'_>) -> Vec<&'a [u8]> {
    info.strip_spans.iter().map(|(s, e)| &req[s..e]).collect()
}

#[test]
fn test_parse_http_headers_fast() {
    let request =
        b"GET /api/v1/users HTTP/1.1\r\nHost: example.com\r\nUser-Agent: curl/7.68.0\r\n\r\n";

    let result = extract_routing_info(request).unwrap();
    assert_eq!(result.path, "/api/v1/users");
    assert_eq!(result.host, "example.com");
}

#[test]
fn test_parse_http_headers_fast_post() {
    let request = b"POST /api/data HTTP/1.1\r\nHost: api.example.com\r\nContent-Type: application/json\r\n\r\n";

    let result = extract_routing_info(request).unwrap();
    assert_eq!(result.path, "/api/data");
    assert_eq!(result.host, "api.example.com");
}

#[test]
fn test_parse_http_headers_fast_no_host() {
    // HTTP/1.1 requires a Host header per RFC 7230 §5.4
    let request = b"GET /api HTTP/1.1\r\nContent-Type: application/json\r\n\r\n";
    let result = extract_routing_info(request);
    assert!(
        result.is_err(),
        "HTTP/1.1 without Host header should be rejected"
    );
}

#[test]
fn test_parse_http_headers_fast_complex_path() {
    let request =
        b"GET /api/v1/users/123?filter=active&sort=name HTTP/1.1\r\nHost: api.example.com\r\n\r\n";

    let result = extract_routing_info(request).unwrap();
    assert_eq!(result.path, "/api/v1/users/123");
    assert_eq!(result.query_string, "filter=active&sort=name");
    assert_eq!(result.host, "api.example.com");
}

#[test]
fn test_parse_path_without_query_string() {
    let request = b"GET /api/users HTTP/1.1\r\nHost: example.com\r\n\r\n";

    let result = extract_routing_info(request).unwrap();
    assert_eq!(result.path, "/api/users");
    assert_eq!(result.query_string, "");
}

#[test]
fn test_parse_path_with_empty_query_string() {
    let request = b"GET /api/users? HTTP/1.1\r\nHost: example.com\r\n\r\n";

    let result = extract_routing_info(request).unwrap();
    assert_eq!(result.path, "/api/users");
    assert_eq!(result.query_string, "");
}

#[test]
fn test_parse_http_headers_fast_root_path() {
    let request = b"GET / HTTP/1.1\r\nHost: example.com\r\n\r\n";

    let result = extract_routing_info(request).unwrap();
    assert_eq!(result.path, "/");
    assert_eq!(result.host, "example.com");
}

#[test]
fn test_parse_http_headers_fast_host_with_port() {
    let request = b"GET /api HTTP/1.1\r\nHost: example.com:8080\r\n\r\n";

    let result = extract_routing_info(request).unwrap();
    assert_eq!(result.path, "/api");
    assert_eq!(result.host, "example.com"); // Normalized - port removed for Gateway API compliance
}

#[test]
fn test_parse_http_headers_fast_case_insensitive_host() {
    let request = b"GET /api HTTP/1.1\r\nHOST: example.com\r\n\r\n";

    let result = extract_routing_info(request).unwrap();
    assert_eq!(result.path, "/api");
    assert_eq!(result.host, "example.com");
}

#[test]
fn test_parse_http_headers_fast_multiple_headers() {
    let request = b"GET /api HTTP/1.1\r\nUser-Agent: test\r\nHost: example.com\r\nAccept: application/json\r\n\r\n";

    let result = extract_routing_info(request).unwrap();
    assert_eq!(result.path, "/api");
    assert_eq!(result.host, "example.com");
}

#[test]
fn test_parse_http_headers_fast_whitespace_in_headers() {
    let request = b"GET /api HTTP/1.1\r\nHost:    example.com   \r\n\r\n";

    let result = extract_routing_info(request).unwrap();
    assert_eq!(result.path, "/api");
    assert_eq!(result.host, "example.com");
}

#[test]
fn test_parse_http_headers_fast_different_methods() {
    let methods = ["GET", "POST", "PUT", "DELETE", "PATCH", "HEAD", "OPTIONS"];

    for method in &methods {
        let request = format!("{} /test HTTP/1.1\r\nHost: example.com\r\n\r\n", method);
        let result = extract_routing_info(request.as_bytes()).unwrap();
        assert_eq!(result.path, "/test");
        assert_eq!(result.host, "example.com");
    }
}

#[test]
fn test_parse_http_headers_fast_empty_request() {
    let request = b"";
    let result = extract_routing_info(request);
    assert!(result.is_err(), "empty request should be rejected");
}

#[test]
fn test_parse_http_headers_fast_minimal_valid_request() {
    // HTTP/1.1 without Host header is invalid per RFC 7230 §5.4
    let request = b"GET / HTTP/1.1\r\n\r\n";
    let result = extract_routing_info(request);
    assert!(
        result.is_err(),
        "HTTP/1.1 without Host header should be rejected"
    );
}

#[test]
fn test_connection_close_header() {
    let request = b"GET /api HTTP/1.1\r\nHost: example.com\r\nConnection: close\r\n\r\n";

    let result = extract_routing_info(request).unwrap();
    assert!(!result.keepalive);
    assert_eq!(result.host, "example.com");
    assert_eq!(result.path, "/api");
}

#[test]
fn test_connection_close_with_trailing_whitespace() {
    let request = b"GET /api HTTP/1.1\r\nHost: example.com\r\nConnection: close \r\n\r\n";

    let result = extract_routing_info(request).unwrap();
    assert!(!result.keepalive);
    assert_eq!(result.host, "example.com");
    assert_eq!(result.path, "/api");
}

#[test]
fn test_connection_keep_alive_header() {
    let request = b"GET /api HTTP/1.1\r\nHost: example.com\r\nConnection: keep-alive\r\n\r\n";

    let result = extract_routing_info(request).unwrap();
    assert!(result.keepalive);
    assert_eq!(result.host, "example.com");
    assert_eq!(result.path, "/api");
}

#[test]
fn test_connection_default_behavior() {
    let request = b"GET /api HTTP/1.1\r\nHost: example.com\r\n\r\n";

    let result = extract_routing_info(request).unwrap();
    assert!(result.keepalive);
    assert_eq!(result.host, "example.com");
    assert_eq!(result.path, "/api");
}

#[test]
fn test_http10_default_close_behavior() {
    let request = b"GET /api HTTP/1.0\r\nHost: example.com\r\n\r\n";

    let result = extract_routing_info(request).unwrap();
    assert!(!result.keepalive);
    assert_eq!(result.host, "example.com");
    assert_eq!(result.path, "/api");
}

#[test]
fn test_http11_default_keepalive_behavior() {
    let request = b"GET /api HTTP/1.1\r\nHost: example.com\r\n\r\n";

    let result = extract_routing_info(request).unwrap();
    assert!(result.keepalive);
    assert_eq!(result.host, "example.com");
    assert_eq!(result.path, "/api");
}

#[test]
fn test_apachebench_scenario() {
    let request = b"GET /index.html HTTP/1.0\r\nHost: localhost:8080\r\nUser-Agent: ApacheBench/2.3\r\nAccept: */*\r\n\r\n";

    let result = extract_routing_info(request).unwrap();
    assert!(!result.keepalive);
    assert_eq!(result.host, "localhost");
    assert_eq!(result.path, "/index.html");
}

// --- RFC 7230 §3.3.3 framing-header smuggling guards ------------------------

#[test]
fn te_chunked_alone_is_accepted() {
    let request = b"POST /up HTTP/1.1\r\nHost: h\r\nTransfer-Encoding: chunked\r\n\r\n";
    let result = extract_routing_info(request).unwrap();
    assert!(result.is_chunked);
    assert!(!result.header_violation);
}

#[test]
fn te_with_cl_is_a_violation() {
    let request =
        b"POST /up HTTP/1.1\r\nHost: h\r\nContent-Length: 4\r\nTransfer-Encoding: chunked\r\n\r\n";
    let result = extract_routing_info(request).unwrap();
    assert!(result.header_violation);
}

#[test]
fn te_gzip_chunked_is_a_violation() {
    // `gzip, chunked` used to slip past the exact-match check, leaving the
    // body framed by CL here while a TE-aware backend frames it chunked
    // (TE.CL desync).
    let request = b"POST /up HTTP/1.1\r\nHost: h\r\nContent-Length: 4\r\nTransfer-Encoding: gzip, chunked\r\n\r\nabcd";
    let result = extract_routing_info(request).unwrap();
    assert!(result.header_violation);
}

#[test]
fn te_identity_is_a_violation() {
    let request = b"POST /up HTTP/1.1\r\nHost: h\r\nTransfer-Encoding: identity\r\n\r\n";
    let result = extract_routing_info(request).unwrap();
    assert!(result.header_violation);
}

#[test]
fn te_repeated_chunked_is_a_violation() {
    let request = b"POST /up HTTP/1.1\r\nHost: h\r\nTransfer-Encoding: chunked\r\nTransfer-Encoding: chunked\r\n\r\n";
    let result = extract_routing_info(request).unwrap();
    assert!(result.header_violation);
}

#[test]
fn duplicate_conflicting_cl_is_a_violation() {
    let request = b"POST /up HTTP/1.1\r\nHost: h\r\nContent-Length: 4\r\nContent-Length: 5\r\n\r\n";
    let result = extract_routing_info(request).unwrap();
    assert!(result.header_violation);
}

#[test]
fn duplicate_equal_cl_is_a_violation() {
    // A backend that merges the lines into `4, 4` cannot parse a length.
    let request = b"POST /up HTTP/1.1\r\nHost: h\r\nContent-Length: 4\r\nContent-Length: 4\r\n\r\n";
    let result = extract_routing_info(request).unwrap();
    assert!(result.header_violation);
}

#[test]
fn upgrade_requires_connection_token() {
    // Bare Upgrade header without Connection: upgrade must not switch protocols.
    let request = b"GET /ws HTTP/1.1\r\nHost: h\r\nUpgrade: websocket\r\n\r\n";
    let result = extract_routing_info(request).unwrap();
    assert!(!result.is_upgrade);

    let request =
        b"GET /ws HTTP/1.1\r\nHost: h\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\r\n";
    let result = extract_routing_info(request).unwrap();
    assert!(result.is_upgrade);
}

#[test]
fn connection_token_list_parsed() {
    // Token list form used by browsers.
    let request =
        b"GET /ws HTTP/1.1\r\nHost: h\r\nUpgrade: websocket\r\nConnection: keep-alive, Upgrade\r\n\r\n";
    let result = extract_routing_info(request).unwrap();
    assert!(result.is_upgrade);
    assert!(result.keepalive);

    // close token still wins inside a list.
    let request = b"GET / HTTP/1.1\r\nHost: h\r\nConnection: close\r\n\r\n";
    let result = extract_routing_info(request).unwrap();
    assert!(!result.keepalive);
}

// --- Strict request head: anything a lenient backend could read differently

#[test]
fn request_line_must_be_exactly_three_single_space_separated_tokens() {
    for line in [
        "GET  / HTTP/1.1",
        "GET /  HTTP/1.1",
        " GET / HTTP/1.1",
        "GET / HTTP/1.1 ",
        "GET\t/ HTTP/1.1",
        "GET /\tHTTP/1.1",
        "GET / HTTP/1.1 extra",
        "GET /a b HTTP/1.1",
        "GET / ",
        "GET /",
        "GET",
        "GET HTTP/1.1",
        "/ HTTP/1.1",
        "",
    ] {
        assert_rejected(&head(line, "Host: h\r\n"));
    }
}

#[test]
fn only_http_1_1_and_1_0_are_accepted() {
    for version in [
        "HTTP/1.2",
        "HTTP/2.0",
        "HTTP/0.9",
        "http/1.1",
        "HTTP/1.1x",
        "HTTP/1.10",
        "HTTP/1",
        "HTTP/11",
        "HTTPS/1.1",
    ] {
        assert_rejected(&head(&format!("GET / {version}"), "Host: h\r\n"));
    }
}

#[test]
fn method_must_be_a_token() {
    for method in [
        &b"GE(T"[..],
        b"G@T",
        b"GET,",
        b"G\"T",
        b"G[T",
        b"GE\x00T",
        b"GE\x7fT",
        b"G\xc3\x89T",
    ] {
        let mut req = method.to_vec();
        req.extend_from_slice(b" / HTTP/1.1\r\nHost: h\r\n\r\n");
        assert_rejected(&req);
    }
}

#[test]
fn target_rejects_ctl_and_del() {
    for target in [
        &b"/a\x00b"[..],
        b"/a\x01b",
        b"/a\x1fb",
        b"/a\x7fb",
        b"/a\tb",
        b"/a\rb",
        b"/a\nb",
        b"/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\x7f",
    ] {
        let mut req = b"GET ".to_vec();
        req.extend_from_slice(target);
        req.extend_from_slice(b" HTTP/1.1\r\nHost: h\r\n\r\n");
        assert_rejected(&req);
    }
}

#[test]
fn target_rejects_fragment_and_backslash() {
    // Routing reads `/admin#x` whole, so it misses an /admin prefix rule;
    // a backend that drops the fragment, or reads '\' as '/', then serves
    // the path that rule guards.
    let pad = "a".repeat(32);
    for target in [
        "/admin#x".to_string(),
        "/admin#".to_string(),
        "#".to_string(),
        "/a?b=#c".to_string(),
        "/admin\\x".to_string(),
        "\\admin".to_string(),
        "/a?b=\\c".to_string(),
        format!("/{pad}#"),
        format!("/{pad}\\"),
        format!("/{pad}#{pad}"),
        "http://h/admin#x".to_string(),
        "http://h\\@evil/".to_string(),
    ] {
        assert_rejected(&head(&format!("GET {target} HTTP/1.1"), "Host: h\r\n"));
    }
    // Percent-encoded they are data, not delimiters.
    let req = head("GET /a%23b%5Cc?d=%23 HTTP/1.1", "Host: h\r\n");
    let info = accepted(&req);
    assert_eq!(info.path, "/a%23b%5Cc");
    assert_eq!(info.query_string, "d=%23");
}

#[test]
fn non_utf8_target_is_an_error() {
    assert!(extract_routing_info(b"GET /\xff HTTP/1.1\r\nHost: h\r\n\r\n").is_err());
}

#[test]
fn target_must_take_a_form_its_method_allows() {
    for line in [
        "GET example.com/ HTTP/1.1",
        "GET ?x HTTP/1.1",
        "GET * HTTP/1.1",
        "OPTIONS *x HTTP/1.1",
        "GET example.com:443 HTTP/1.1",
        "GET ftp://example.com/ HTTP/1.1",
        "GET http:/example.com/ HTTP/1.1",
        "CONNECT / HTTP/1.1",
        "CONNECT * HTTP/1.1",
        "CONNECT http://example.com/ HTTP/1.1",
        "CONNECT example.com HTTP/1.1",
        "CONNECT example.com: HTTP/1.1",
        "CONNECT :443 HTTP/1.1",
        "CONNECT user@example.com:443 HTTP/1.1",
    ] {
        assert_rejected(&head(line, "Host: example.com\r\n"));
    }
}

#[test]
fn every_line_must_end_in_crlf() {
    for req in [
        &b"GET / HTTP/1.1\nHost: h\r\n\r\n"[..],
        b"GET / HTTP/1.1\rHost: h\r\n\r\n",
        b"GET / HTTP/1.1\r\nHost: h\nX-A: b\r\n\r\n",
        b"GET / HTTP/1.1\r\nHost: h\rX-A: b\r\n\r\n",
        b"GET / HTTP/1.1\r\nHost: h\r\r\n\r\n",
        b"GET / HTTP/1.1\r\nHost: h\n\r\n",
        b"GET / HTTP/1.1\r\nHost: h\r\n\n",
        b"GET / HTTP/1.1\r\nHost: h\r\n\rX-A: b\r\n\r\n",
    ] {
        assert_rejected(req);
    }
}

#[test]
fn head_without_its_blank_line_is_rejected() {
    for req in [
        &b"GET / HTTP/1.1\r\n"[..],
        b"GET / HTTP/1.1\r\nHost: h\r\n",
        b"GET / HTTP/1.1\r\nHost: h\r\n\r",
        b"GET / HTTP/1.1\r\nHost: h",
        b"GET / HTTP/1.0\r\n",
    ] {
        assert_rejected(req);
    }
}

#[test]
fn header_line_needs_a_token_name_and_a_colon() {
    for line in [
        &b"X-No-Colon"[..],
        b": empty-name",
        b"Content-Length : 5",
        b"Transfer-Encoding : chunked",
        b"Content-Length\t: 5",
        b"X Y: z",
        b"X(Y): z",
        b"X/Y: z",
        b"X@Y: z",
        b"X\"Y: z",
        b"X\x00Y: z",
        b"X\xc3\xa9: z",
    ] {
        assert_rejected(&with_header(line));
    }
}

#[test]
fn obs_fold_is_rejected() {
    assert_rejected(b"GET / HTTP/1.1\r\nHost: h\r\nX-A: a\r\n b\r\n\r\n");
    assert_rejected(b"GET / HTTP/1.1\r\nHost: h\r\nX-A: a\r\n\tb\r\n\r\n");
    assert_rejected(b"GET / HTTP/1.1\r\n Host: h\r\n\r\n");
    assert_rejected(b"GET / HTTP/1.1\r\n\tHost: h\r\n\r\n");
}

#[test]
fn field_value_rejects_every_ctl_but_htab() {
    for ctl in (0u8..0x20).filter(|&b| b != b'\t').chain([0x7f]) {
        let pad = "v".repeat(20);
        for (before, after) in [("", ""), ("a", "b"), (pad.as_str(), pad.as_str())] {
            let mut line = format!("X-A: {before}").into_bytes();
            line.push(ctl);
            line.extend_from_slice(after.as_bytes());
            assert_rejected(&with_header(&line));
        }
    }
}

#[test]
fn content_length_must_be_one_or_more_digits() {
    for value in [
        "+5",
        "-1",
        "5, 5",
        "5,5",
        "",
        "  ",
        "5 5",
        "0x5",
        "5.0",
        "1e3",
        "\u{0665}",
        "99999999999999999999999999",
    ] {
        assert_rejected(&with_header(format!("Content-Length: {value}").as_bytes()));
    }
}

#[test]
fn repeated_host_is_a_violation() {
    assert_rejected(b"GET / HTTP/1.1\r\nHost: a.example\r\nHost: b.example\r\n\r\n");
    assert_rejected(b"GET / HTTP/1.1\r\nHost: a.example\r\nhost: a.example\r\n\r\n");
    assert_rejected(b"GET / HTTP/1.0\r\nHost: a.example\r\nHost: a.example\r\n\r\n");
}

#[test]
fn host_must_be_host_and_optional_port() {
    for value in [
        &b"a b"[..],
        b"a/b",
        b"a?b",
        b"a#b",
        b"user@a",
        b"a\\b",
        b"a\"b",
        b"a<b",
        b"a:8x",
        b"a:80:80",
        b"a:-1",
        b"a:+80",
        b"a[b",
        b"a]",
        b"[::1",
        b"[]",
        b"[::1]x",
        b"[::1]:x",
        b"[::1]]",
        b"::1",
        b"caf\xc3\xa9.example",
        b"a\x7f",
    ] {
        let mut req = b"GET / HTTP/1.1\r\nHost: ".to_vec();
        req.extend_from_slice(value);
        req.extend_from_slice(b"\r\n\r\n");
        assert_rejected(&req);
    }
}

#[test]
fn empty_host_is_a_violation_on_http11_only() {
    assert_rejected(b"GET / HTTP/1.1\r\nHost:\r\n\r\n");
    assert_rejected(b"GET / HTTP/1.1\r\nHost:  \t \r\n\r\n");
    assert_rejected(b"GET / HTTP/1.1\r\nHost: :80\r\n\r\n");
    assert_eq!(accepted(b"GET / HTTP/1.0\r\nHost:\r\n\r\n").host, "");
    assert_eq!(accepted(b"GET / HTTP/1.0\r\n\r\n").host, "");
}

#[test]
fn absolute_form_userinfo_is_rejected() {
    assert_rejected(b"GET http://user@example.com/ HTTP/1.1\r\nHost: example.com\r\n\r\n");
    assert_rejected(b"GET http://u:p@example.com/ HTTP/1.1\r\nHost: example.com\r\n\r\n");
    assert_rejected(b"GET http://@example.com/ HTTP/1.1\r\nHost: example.com\r\n\r\n");
}

#[test]
fn absolute_form_authority_must_name_the_host_header_host() {
    for req in [
        &b"GET http://internal.example/admin HTTP/1.1\r\nHost: public.example\r\n\r\n"[..],
        b"GET https://internal.example HTTP/1.1\r\nHost: public.example:443\r\n\r\n",
        // A parser ending the authority at '?' or '#' reads example.com, one
        // ending it at '/' reads evil.example: refuse both readings.
        b"GET http://example.com?@evil.example/ HTTP/1.1\r\nHost: example.com\r\n\r\n",
        b"GET http://example.com#@evil.example/ HTTP/1.1\r\nHost: example.com\r\n\r\n",
        b"GET http://example.com\\@evil.example/ HTTP/1.1\r\nHost: example.com\r\n\r\n",
        b"GET http:///x HTTP/1.1\r\nHost: example.com\r\n\r\n",
        // No Host to agree with.
        b"GET http://example.com/ HTTP/1.0\r\n\r\n",
    ] {
        assert_rejected(req);
    }
}

#[test]
fn connect_authority_must_name_the_host_header_host() {
    assert_rejected(b"CONNECT internal.example:443 HTTP/1.1\r\nHost: public.example\r\n\r\n");
}

#[test]
fn connection_cannot_nominate_framing_or_routing_fields() {
    for req in [
        &b"POST / HTTP/1.1\r\nHost: h\r\nConnection: close, Content-Length\r\nContent-Length: 5\r\n\r\n"[..],
        b"POST / HTTP/1.1\r\nHost: h\r\nConnection: Transfer-Encoding\r\nTransfer-Encoding: chunked\r\n\r\n",
        b"GET / HTTP/1.1\r\nHost: h\r\nConnection: keep-alive, HOST\r\n\r\n",
    ] {
        assert_rejected(req);
    }
}

#[test]
fn transfer_encoding_on_http10_is_a_violation() {
    assert_rejected(b"POST / HTTP/1.0\r\nHost: h\r\nTransfer-Encoding: chunked\r\n\r\n");
}

#[test]
fn h2c_upgrade_is_stripped_not_honored() {
    let req = b"GET / HTTP/1.1\r\nHost: h\r\nConnection: Upgrade, HTTP2-Settings\r\nUpgrade: h2c\r\nHTTP2-Settings: AAMAAABkAARAAAAAAAIAAAAA\r\nAccept: */*\r\n\r\n";
    let info = accepted(req);
    assert!(!info.is_upgrade);
    assert!(info.upgrade_span.is_none());
    assert!(!info.strip_spans.needs_slow_path);
    assert_eq!(
        stripped(req, &info),
        [
            &b"Connection: Upgrade, HTTP2-Settings\r\n"[..],
            b"Upgrade: h2c\r\n",
            b"HTTP2-Settings: AAMAAABkAARAAAAAAAIAAAAA\r\n",
        ]
    );
}

#[test]
fn h2c_anywhere_in_upgrade_cancels_the_upgrade() {
    for upgrade in [
        "Upgrade: websocket, H2C\r\n",
        "Upgrade: h2c/1\r\n",
        "Upgrade: websocket\r\nUpgrade: h2c\r\n",
        "Upgrade: h2c\r\nUpgrade: websocket\r\n",
    ] {
        let req = head(
            "GET / HTTP/1.1",
            &format!("Host: h\r\nConnection: Upgrade\r\n{upgrade}"),
        );
        assert!(!accepted(&req).is_upgrade, "{upgrade:?}");
    }
}

#[test]
fn stray_http2_settings_is_stripped() {
    let req = b"GET / HTTP/1.1\r\nHost: h\r\nhttp2-settings: x\r\n\r\n";
    assert_eq!(
        stripped(req, &accepted(req)),
        [&b"http2-settings: x\r\n"[..]]
    );
}

#[test]
fn repeated_upgrade_lines_take_the_slow_path() {
    let req = b"GET / HTTP/1.1\r\nHost: h\r\nUpgrade: websocket\r\nUpgrade: websocket\r\n\r\n";
    assert!(accepted(req).strip_spans.needs_slow_path);
}

#[test]
fn client_x_forwarded_proto_is_stripped() {
    let req = b"GET / HTTP/1.1\r\nHost: h\r\nX-Forwarded-Proto: https\r\nAccept: */*\r\nx-forwarded-proto: https\r\n\r\n";
    let info = accepted(req);
    assert!(!info.strip_spans.needs_slow_path);
    assert_eq!(
        stripped(req, &info),
        [
            &b"X-Forwarded-Proto: https\r\n"[..],
            b"x-forwarded-proto: https\r\n",
        ]
    );
}

#[test]
fn value_edge_cases_are_accepted() {
    let req = b"GET / HTTP/1.1\r\nHost: example.com \t\r\nX-Tab: a\tb\r\nX-Obs: caf\xc3\xa9 \x80\xff\r\nX-Empty:\r\nX-Blank:  \t \r\nX-Long: 0123456789abcdef0123456789abcdef\t0123456789abcdef\r\n\r\n";
    let info = accepted(req);
    assert_eq!(info.host, "example.com");
    assert_eq!(info.path, "/");
}

#[test]
fn host_port_and_ip_literal_are_accepted() {
    for (value, host) in [
        ("example.com:8080", "example.com"),
        ("Example.COM", "Example.COM"),
        ("example.com:", "example.com"),
        ("[::1]:8080", "[::1]"),
        ("[::1]", "[::1]"),
        ("[fe80::1%25eth0]:443", "[fe80::1%25eth0]"),
        ("192.0.2.1:80", "192.0.2.1"),
        ("xn--caf-dma.example", "xn--caf-dma.example"),
        ("a_b-c~d.example", "a_b-c~d.example"),
    ] {
        let req = head("GET / HTTP/1.1", &format!("Host: {value}\r\n"));
        assert_eq!(accepted(&req).host, host, "{value}");
    }
}

#[test]
fn lowercase_header_names_are_recognized() {
    let info =
        accepted(b"POST / HTTP/1.1\r\nhost: h\r\ncontent-length: 3\r\nconnection: close\r\n\r\n");
    assert_eq!(info.host, "h");
    assert_eq!(info.content_length, Some(3));
    assert!(!info.keepalive);
    assert!(
        accepted(b"POST / HTTP/1.1\r\nhost: h\r\ntransfer-encoding: chunked\r\n\r\n").is_chunked
    );
}

#[test]
fn content_length_tolerates_ows_and_leading_zeros() {
    let info = accepted(b"POST / HTTP/1.1\r\nHost: h\r\nContent-Length:\t 007 \t\r\n\r\n");
    assert_eq!(info.content_length, Some(7));
    let info = accepted(b"POST / HTTP/1.1\r\nHost: h\r\nContent-Length: 0\r\n\r\n");
    assert_eq!(info.content_length, Some(0));
}

#[test]
fn options_asterisk_form_is_accepted() {
    let info = accepted(b"OPTIONS * HTTP/1.1\r\nHost: h\r\n\r\n");
    assert_eq!(info.method, "OPTIONS");
    assert_eq!(info.path, "*");
}

#[test]
fn connect_authority_form_is_accepted() {
    let info = accepted(b"CONNECT host:443 HTTP/1.1\r\nHost: host:443\r\n\r\n");
    assert_eq!(info.method, "CONNECT");
    assert_eq!(info.host, "host");
    let info = accepted(b"CONNECT [::1]:443 HTTP/1.1\r\nHost: [::1]\r\n\r\n");
    assert_eq!(info.host, "[::1]");
}

#[test]
fn absolute_form_naming_the_host_routes_by_its_path() {
    let info =
        accepted(b"GET http://Example.com:8080/a/b?c=/d HTTP/1.1\r\nHost: example.com\r\n\r\n");
    assert_eq!(info.host, "example.com");
    assert_eq!(info.path, "/a/b");
    assert_eq!(info.query_string, "c=/d");
    let info = accepted(b"GET HTTPS://example.com HTTP/1.1\r\nHost: example.com:443\r\n\r\n");
    assert_eq!(info.path, "/");
    assert_eq!(info.query_string, "");
}

#[test]
fn websocket_upgrade_with_connection_list_is_accepted() {
    let req = b"GET /ws HTTP/1.1\r\nHost: h\r\nUpgrade: websocket\r\nConnection: keep-alive, Upgrade\r\nSec-WebSocket-Version: 13\r\n\r\n";
    let info = accepted(req);
    assert!(info.is_upgrade);
    assert!(info.keepalive);
    let (s, e) = info.upgrade_span.unwrap();
    assert_eq!(&req[s as usize..e as usize], b"Upgrade: websocket\r\n");
    assert_eq!(
        stripped(req, &info),
        [&b"Connection: keep-alive, Upgrade\r\n"[..]]
    );
}

#[test]
fn utf8_and_long_targets_are_accepted() {
    let info = accepted("GET /caf\u{e9}?q=\u{e9} HTTP/1.1\r\nHost: h\r\n\r\n".as_bytes());
    assert_eq!(info.path, "/caf\u{e9}");
    assert_eq!(info.query_string, "q=\u{e9}");
    let long = format!("/{}", "x".repeat(4000));
    let req = head(&format!("GET {long}?a=b HTTP/1.1"), "Host: h\r\n");
    assert_eq!(accepted(&req).path, long);
}

#[test]
fn body_bytes_after_the_head_are_not_parsed() {
    let info = accepted(b"POST / HTTP/1.1\r\nHost: h\r\nContent-Length: 4\r\n\r\n\x00\n\rX");
    assert_eq!(info.content_length, Some(4));
}

/// Any accepted head must end at the first CRLFCRLF, which is where the
/// worker cuts the head it forwards; spans must cover whole header lines.
fn check_never_panics_and_frames_like_the_worker(req: &[u8]) {
    let Ok(info) = extract_routing_info(req) else {
        return;
    };
    if info.header_violation {
        return;
    }
    let head_end =
        info.header_data.as_ptr() as usize - req.as_ptr() as usize + info.header_data.len();
    let first_blank = req.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 2;
    assert_eq!(head_end, first_blank, "{:?}", String::from_utf8_lossy(req));
    let header_start = head_end - info.header_data.len();
    let spans: Vec<(usize, usize)> = info.strip_spans.iter().collect();
    assert!(spans.windows(2).all(|w| w[0].1 <= w[1].0));
    let upgrade = info.upgrade_span.map(|(s, e)| (s as usize, e as usize));
    for (s, e) in spans.into_iter().chain(upgrade) {
        assert!(header_start <= s && s < e && e <= head_end);
        assert_eq!(&req[s - 2..s], b"\r\n");
        assert_eq!(&req[e - 2..e], b"\r\n");
    }
}

#[test]
fn mutated_heads_never_panic_and_frame_like_the_worker() {
    let seeds: [&[u8]; 4] = [
        b"GET /a?b=c HTTP/1.1\r\nHost: example.com:8080\r\nConnection: keep-alive, Upgrade\r\nUpgrade: websocket\r\nX-Long: 0123456789abcdef0123\r\n\r\n",
        b"POST http://example.com/x HTTP/1.1\r\nHost: example.com\r\nContent-Length: 5\r\nX-Forwarded-Proto: https\r\n\r\nhello",
        b"CONNECT [::1]:443 HTTP/1.1\r\nHost: [::1]:443\r\nTransfer-Encoding: chunked\r\nTE: trailers\r\n\r\n",
        b"OPTIONS * HTTP/1.0\r\nExpect: 100-continue\r\nUpgrade: h2c\r\nHTTP2-Settings: x\r\n\r\n",
    ];
    let bytes = [
        0x00, b'\t', b'\n', b'\r', b' ', b'"', b'#', b'%', b',', b'/', b':', b'?', b'@', b'[',
        b'\\', b']', 0x7f, 0x80, 0xff,
    ];
    for seed in seeds {
        accepted(seed);
        for cut in 0..=seed.len() {
            check_never_panics_and_frames_like_the_worker(&seed[..cut]);
        }
        for i in 0..seed.len() {
            for &b in &bytes {
                let mut m = seed.to_vec();
                m[i] = b;
                check_never_panics_and_frames_like_the_worker(&m);
                let mut m = seed.to_vec();
                m.insert(i, b);
                check_never_panics_and_frames_like_the_worker(&m);
            }
            let mut m = seed.to_vec();
            m.remove(i);
            check_never_panics_and_frames_like_the_worker(&m);
        }
    }
}
