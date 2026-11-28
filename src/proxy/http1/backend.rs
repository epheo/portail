//! A backend connection for one request, and what the client hears when
//! there is none.
//!
//! Every failure here answers the HTTP/1 client itself. The native h2 path
//! will lift a connect that returns its outcome to `upstream::connect`, and
//! leave the answers with each front. Tests that drive [`super::serve`] stay
//! with this front then.

use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;

use anyhow::Result;

use super::reply::send_error_response;
use super::ConnectionState;
use crate::logging::warn;
use crate::proxy::backend_pool::{log_local_exhaustion, ConnectError};
use crate::proxy::deadline::within;
use crate::proxy::health::HealthRegistry;
use crate::proxy::proxy_protocol;
use crate::proxy::tls::Connection;
use crate::proxy::upstream::connect::{backend_metric_label, connect_proxied};
use crate::routing::BackendAction;

/// A backend connect that failed (`502`) or timed out (`504`): counted as
/// one, then answered as [`fail_backend`] does.
pub(super) async fn fail_connect(
    state: &mut ConnectionState,
    backend_ref: &crate::routing::Backend,
    client: &mut Connection,
    code: u16,
) -> Result<()> {
    let label = backend_metric_label(backend_ref);
    match code {
        504 => {
            crate::metrics::record_backend_connect_timeout(&label);
            state.listener_stats.upstream_connect_timeouts.inc();
        }
        _ => {
            crate::metrics::record_backend_connect_error(&label);
            state.listener_stats.upstream_connect_errors.inc();
        }
    }
    fail_backend(state, client, backend_ref.socket_addr, code).await
}

/// A request that failed to send on a freshly connected backend conn: a
/// send error, not a connect error, then answered `502` as [`fail_backend`]
/// does.
pub(super) async fn fail_send(
    state: &mut ConnectionState,
    backend_ref: &crate::routing::Backend,
    client: &mut Connection,
) -> Result<()> {
    crate::metrics::record_backend_send_error(&backend_metric_label(backend_ref));
    state.listener_stats.upstream_send_errors.inc();
    fail_backend(state, client, backend_ref.socket_addr, 502).await
}

/// Record a backend failure (spawning a health probe if this newly trips the
/// backend unhealthy) and send `code` to the client, whose connection then
/// closes. Log wording stays caller-side.
async fn fail_backend(
    state: &mut ConnectionState,
    client: &mut Connection,
    addr: SocketAddr,
    code: u16,
) -> Result<()> {
    if state.health.record_failure(addr) {
        HealthRegistry::spawn_probe(Arc::clone(&state.health), addr);
    }
    state.refused(code);
    send_error_response(client, code).await
}

/// Answer a connect the proxy could not make for want of its own fds or
/// memory: a 503, and the backend's health left alone.
pub(super) async fn refuse_locally(
    state: &mut ConnectionState,
    client: &mut Connection,
    addr: SocketAddr,
    e: &std::io::Error,
) -> Result<()> {
    log_local_exhaustion(addr, e);
    // Not refused(): a lingering close would hold this connection's fd
    // while the proxy is short of fds or memory.
    state.last_status = 503;
    send_error_response(client, 503).await
}

/// Acquire a backend connection by `deadline`, on `timer` (see `within`).
/// On a backend failure or timeout, records health failure and sends `502`/`504`
/// to the client; on local exhaustion, a `503` (see [`ConnectError::Local`]).
/// Returns `Ok(None)` when the caller should bail out (`return Ok(false)`),
/// which signals the connection is not reusable.
///
/// `Ok(Some((conn, from_pool)))`: `from_pool == true` means the conn came from
/// the idle pool and the caller may retry once with `acquire_fresh` if the
/// backend turns out to have dropped it (the probe is best-effort).
pub(super) async fn connect_to_backend(
    client: &mut Connection,
    state: &mut ConnectionState,
    backend_ref: &crate::routing::Backend,
    timer: Pin<&mut tokio::time::Sleep>,
    deadline: Option<tokio::time::Instant>,
) -> Result<Option<(Connection, bool)>> {
    let backend_addr = backend_ref.socket_addr;
    let proto = backend_ref.proto();
    let acquired = if backend_ref.action == BackendAction::ForwardProxied {
        // Never pooled: its header names this request's client, and a
        // pooled conn would carry it to another client's request.
        let header = proxy_protocol::header(state.peer, state.local);
        let connect = connect_proxied(
            backend_addr,
            state.connect_timeout,
            &state.backend_keepalive,
            header,
        );
        within(timer, deadline, connect).await
    } else {
        match state.pool.take_idle(backend_addr, proto) {
            Some(conn) => Some(Ok((conn, true))),
            None => {
                let keepalive = &state.backend_keepalive;
                let connect = state.pool.acquire(backend_addr, proto, keepalive);
                within(timer, deadline, connect).await
            }
        }
    };
    match acquired {
        Some(Ok(result)) => {
            // Reset passive-failure state: without this, transient failures
            // accumulate over the process lifetime and eventually trip the
            // "consecutive"-failure threshold (the L4 paths already record
            // success on connect).
            state.health.record_success(backend_addr);
            Ok(Some(result))
        }
        Some(Err(ConnectError::Local(e))) => {
            refuse_locally(state, client, backend_addr, &e).await?;
            Ok(None)
        }
        Some(Err(e)) => {
            warn!("Backend {} connect failed: {}", backend_addr, e);
            fail_connect(state, backend_ref, client, 502).await?;
            Ok(None)
        }
        None => {
            warn!("Backend {} connect timeout", backend_addr);
            fail_connect(state, backend_ref, client, 504).await?;
            Ok(None)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proxy::listener::{TCP_KEEPALIVE_INTERVAL, TCP_KEEPALIVE_RETRIES};
    use crate::proxy::testkit::{
        backend, conn_state, conn_state_header, exchange, gateway_backend, idle_backend,
        keepalive_where, read_one_reply, received, routes, routes_with_tcp, serve, serve_state,
        GET_A, OPAQUE, REFUSED,
    };
    use crate::routing::{Backend, HttpRouteRule, PathMatchType, RouteTable};
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    /// An invalid ref's share answers 500; a Gateway backend's before any
    /// of its instances reported answers 503: one is wrong, the other not
    /// up yet.
    #[tokio::test]
    async fn an_unreported_gateway_backend_answers_503_an_invalid_ref_500() {
        for (backend, status) in [
            (Backend::rejecting(1), "500"),
            (Backend::unavailable(1), "503"),
        ] {
            let mut rt = RouteTable::new();
            rt.add_http_route(
                "a.test",
                HttpRouteRule::new(
                    PathMatchType::Prefix,
                    "/".to_string(),
                    vec![],
                    vec![],
                    vec![],
                    vec![backend],
                ),
            );
            let (mut client, _task) = serve(rt);
            let out = exchange(&mut client, b"GET / HTTP/1.1\r\nHost: a.test\r\n\r\n").await;
            assert!(out.starts_with(&format!("HTTP/1.1 {status} ")), "{out}");
        }
    }

    /// Keepalive time, interval and retries of the socket in this process
    /// whose local address is `local`: the proxy end of a backend connection,
    /// as the backend saw its peer. `None` when keepalive is off.
    fn keepalive_of(local: SocketAddr) -> Option<(Duration, Duration, u32)> {
        keepalive_where(|sock| sock.local_addr().ok().and_then(|a| a.as_socket()) == Some(local))
    }

    /// A Gateway backend's connection carries one client's header, so it
    /// serves one request: never from the pool, never back to it.
    #[tokio::test]
    async fn each_request_to_a_gateway_backend_opens_its_own_connection() {
        let (addr, mut rx) = backend(REFUSED).await;
        let mut rt = RouteTable::new();
        rt.add_http_route(
            "a.test",
            HttpRouteRule::new(
                PathMatchType::Prefix,
                "/".to_string(),
                vec![],
                vec![],
                vec![],
                vec![gateway_backend(addr)],
            ),
        );
        let (mut client, _task) = serve(rt);
        for _ in 0..2 {
            client
                .write_all(b"GET / HTTP/1.1\r\nHost: a.test\r\n\r\n")
                .await
                .unwrap();
            let reply = read_one_reply(&mut client).await;
            assert!(reply.starts_with("HTTP/1.1 200 OK\r\n"), "{reply}");
        }
        client.shutdown().await.unwrap();
        for _ in 0..2 {
            let seen = received(&mut rx).await;
            let header = conn_state_header();
            assert_eq!(seen[..header.len()], header[..]);
            let request = String::from_utf8_lossy(&seen[header.len()..]).into_owned();
            assert!(request.starts_with("GET / HTTP/1.1\r\n"), "{request}");
            assert_eq!(request.matches("GET ").count(), 1, "{request}");
        }
    }

    /// A backend that vanishes without FIN or RST would otherwise pin an
    /// idle client's task and both fds forever, as a dead client once did.
    #[tokio::test]
    async fn backend_sockets_get_the_client_keepalive() {
        let want = Some((
            Duration::from_secs(60),
            TCP_KEEPALIVE_INTERVAL,
            TCP_KEEPALIVE_RETRIES,
        ));

        // Checked once the first bytes arrive: the proxy sends them only
        // after setting the socket up, while accept returns before.
        let http = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let (mut client, _task) = serve(routes(http.local_addr().unwrap()));
        client
            .write_all(b"GET / HTTP/1.1\r\nHost: a.test\r\n\r\n")
            .await
            .unwrap();
        let (mut pooled, proxy_end) = http.accept().await.unwrap();
        pooled.read_exact(&mut [0u8; 4]).await.unwrap();
        assert_eq!(keepalive_of(proxy_end), want, "pooled HTTP backend");

        let tcp = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let unused = http.local_addr().unwrap();
        let (mut client, _task) = serve(routes_with_tcp(unused, tcp.local_addr().unwrap()));
        client.write_all(OPAQUE).await.unwrap();
        let (mut relayed, proxy_end) = tcp.accept().await.unwrap();
        relayed.read_exact(&mut [0u8; 4]).await.unwrap();
        assert_eq!(keepalive_of(proxy_end), want, "L4 backend");
    }

    /// Out of fds, the proxy never reaches the backend: the client gets a
    /// 503 and the backend's health is untouched. Counted as backend
    /// failures, one exhausted proxy ejected healthy backends route-wide.
    #[test]
    fn local_exhaustion_is_a_503_not_a_backend_failure() {
        use crate::proxy::test_util::{in_own_process, FdsExhausted};
        if !in_own_process(
            module_path!(),
            "local_exhaustion_is_a_503_not_a_backend_failure",
        ) {
            return;
        }

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let (addr, _rx) = backend(REFUSED).await;
            let unused = idle_backend().await;
            let health = Arc::new(HealthRegistry::new());
            let serve_shared = |rt| {
                let mut state = conn_state(rt, false);
                state.health = Arc::clone(&health);
                serve_state(state)
            };

            let exhausted = FdsExhausted::new();
            for _ in 0..=crate::proxy::health::FAILURE_THRESHOLD {
                let (mut client, _task) = serve_shared(routes(addr));
                let out = exchange(&mut client, GET_A).await;
                assert!(
                    out.starts_with("HTTP/1.1 503 Service Unavailable\r\n"),
                    "{out}"
                );
                // Raw TCP has no status to send: the client is just closed.
                let l4 = routes_with_tcp(unused.local_addr().unwrap(), addr);
                let (mut client, _task) = serve_shared(l4);
                assert_eq!(exchange(&mut client, OPAQUE).await, "");
            }
            assert!(health.is_healthy(&addr));
            drop(exhausted);

            let (mut client, _task) = serve_shared(routes(addr));
            let out = exchange(&mut client, GET_A).await;
            assert!(out.starts_with("HTTP/1.1 200 OK\r\n"), "{out}");
        });
    }

    /// What a snapshot or a file gives: a backendRef with `tls`, and the
    /// bundle it names.
    fn verified(backend: std::net::SocketAddr, hostname: &str, ca_pem: &str) -> RouteTable {
        let config: crate::config::PortailConfig = serde_json::from_value(serde_json::json!({
            "gateway": {"name": "g", "listeners": [{"name": "http", "protocol": "HTTP", "port": 8080}]},
            "backendCas": {"ca": ca_pem},
            "httpRoutes": [{
                "parentRefs": [{"name": "g", "sectionName": "http"}],
                "hostnames": ["a.test"],
                "rules": [{"backendRefs": [{
                    "name": "127.0.0.1",
                    "port": backend.port(),
                    "tls": {"hostname": hostname, "ca": {"bundle": "ca"}}
                }]}]
            }]
        }))
        .unwrap();
        config.validate().unwrap();
        config.to_route_table_strict().unwrap()
    }

    /// No appProtocol on the ref: the policy alone makes it TLS.
    #[tokio::test]
    async fn a_request_reaches_a_backend_verified_under_its_policy() {
        use crate::proxy::testkit::{delivered, response, TestCa, OK};
        let ca = TestCa::new();
        let (backend, mut seen) = ca.backend(&["svc.test"], &[], OK.as_bytes()).await;
        let (mut client, _engine) = serve(verified(backend, "svc.test", &ca.pem()));

        for _ in 0..2 {
            client.write_all(GET_A).await.unwrap();
            assert_eq!(response(&mut client).await, OK);
        }
        drop(client);
        let seen = delivered(&mut seen).await;
        assert_eq!(
            seen.matches("GET / HTTP/1.1").count(),
            2,
            "one conn: {seen}"
        );
    }

    /// Wrong issuer or wrong name: the handshake stops before any request
    /// byte, and the client is told the backend is bad.
    #[tokio::test]
    async fn a_backend_failing_its_policy_gets_no_request_and_the_client_a_502() {
        use crate::proxy::testkit::{delivered, TestCa, OK};
        let (ca, other) = (TestCa::new(), TestCa::new());
        for (issuer, hostname) in [(&other, "svc.test"), (&ca, "else.test")] {
            let (backend, mut seen) = issuer.backend(&["svc.test"], &[], OK.as_bytes()).await;
            let (mut client, _engine) = serve(verified(backend, hostname, &ca.pem()));

            let out = exchange(&mut client, GET_A).await;
            assert!(out.starts_with("HTTP/1.1 502 Bad Gateway\r\n"), "{out}");
            assert_eq!(delivered(&mut seen).await, "");
        }
    }
}
