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
    let (use_tls, server_name) = (backend_ref.use_tls, backend_ref.server_name.as_str());
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
        match state.pool.take_idle(backend_addr, use_tls, server_name) {
            Some(conn) => Some(Ok((conn, true))),
            None => {
                let keepalive = &state.backend_keepalive;
                let connect = state
                    .pool
                    .acquire(backend_addr, use_tls, server_name, keepalive);
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
