//! A controller for data-plane tests: `POST /v1/sync` over TLS from a
//! throwaway CA, answered from a script the test changes, every request
//! and answer recorded. It can go down and come back on its address. It
//! signs its snapshots and gone answers as the controller does.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use portail::config::PortailConfig;
use portail::sync::{sign, GatewayRef, Gone, Signed, Snapshot, SyncRequest};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use rustls::sign::CertifiedKey;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

/// The Gateway every stub snapshot is for.
pub const GATEWAY: &str = "default/gw";

/// The controller Service the stub's leaf also names, as a data plane
/// polling through the apiserver checks it.
pub const CONTROLLER_NAME: &str = "portail-controller.portail-system.svc";

/// What the stub answers a poll; a leaving one gets 204 at once, as from
/// the controller.
// Set a few times per test: the snapshot's size inline costs nothing.
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Debug)]
pub enum Mode {
    /// The snapshot at once when the poll's `have` differs from its
    /// version, else 204 at the end of the hold. `None`: hold, then 204,
    /// as for a Gateway whose first translation is pending.
    Serve(Option<Snapshot>),
    /// 404.
    Gone,
    /// 503, with `Retry-After` when given.
    Unavailable(Option<u64>),
    /// As `Serve`, the snapshot signed by a leaf of another CA: what an
    /// endpoint impersonating the controller could send.
    Forged(Snapshot),
    /// As `Serve` with `sent`, under the controller's signature of
    /// `signed`: a body changed on its way.
    Tampered { signed: Snapshot, sent: Snapshot },
    /// The first snapshot answer sent, again, as recorded: a replay to a
    /// later poll.
    Replay,
}

/// A request as it arrived.
#[derive(Clone, Debug)]
pub struct Recorded {
    pub at: Instant,
    pub path: String,
    /// Which TLS connection it came on, numbered from 1 in accept order.
    pub connection: u64,
    pub authorization: Option<String>,
    /// X-Portail-Token, which a poll through the apiserver carries.
    pub token: Option<String>,
    pub request: SyncRequest,
}

/// An answer as it left.
#[derive(Clone, Debug)]
pub struct Answered {
    pub at: Instant,
    pub connection: u64,
    pub status: StatusCode,
}

struct State {
    mode: watch::Sender<Mode>,
    /// The serving leaf, which signs answers.
    signer: CertifiedKey,
    /// Another CA's leaf for the same address.
    forger: CertifiedKey,
    /// The first 200 sent: its headers and body.
    first_snapshot: Mutex<Option<(hyper::HeaderMap, Vec<u8>)>>,
    hold: Duration,
    requests: Mutex<Vec<Recorded>>,
    answers: Mutex<Vec<Answered>>,
    connections: AtomicU64,
}

pub struct ControllerStub {
    pub addr: SocketAddr,
    pub ca_pem: String,
    tls: Arc<rustls::ServerConfig>,
    state: Arc<State>,
    runtime: tokio::runtime::Runtime,
    /// Cancelled to go down: the listener and every connection close.
    up: Mutex<Option<CancellationToken>>,
}

impl ControllerStub {
    /// Up on a loopback port, holding unchanged polls for `hold`.
    pub fn start(hold: Duration, mode: Mode) -> Self {
        let stub = Self::down(hold, mode);
        stub.up();
        stub
    }

    /// Down at first, on an address it takes when [`Self::up`].
    pub fn down(hold: Duration, mode: Mode) -> Self {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        let addr = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap();
        let (ca_pem, tls, signer) = test_pki(addr.ip());
        let (_, _, forger) = test_pki(addr.ip());
        Self {
            addr,
            ca_pem,
            tls: Arc::new(tls),
            state: Arc::new(State {
                mode: watch::Sender::new(mode),
                signer,
                forger,
                first_snapshot: Mutex::default(),
                hold,
                requests: Mutex::default(),
                answers: Mutex::default(),
                connections: AtomicU64::new(0),
            }),
            runtime: tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .unwrap(),
            up: Mutex::new(None),
        }
    }

    pub fn url(&self) -> String {
        format!("https://{}", self.addr)
    }

    /// Serve again on the same address.
    pub fn up(&self) {
        let token = CancellationToken::new();
        let listener = self.runtime.block_on(async {
            // The port was ours a moment ago: give a lingering socket of
            // another test's a few tries.
            for _ in 0..50 {
                if let Ok(listener) = tokio::net::TcpListener::bind(self.addr).await {
                    return listener;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            panic!("controller stub: {} stays taken", self.addr);
        });
        let acceptor = tokio_rustls::TlsAcceptor::from(self.tls.clone());
        let state = self.state.clone();
        let stop = token.clone();
        self.runtime.spawn(async move {
            loop {
                let (tcp, _) = tokio::select! {
                    _ = stop.cancelled() => return,
                    accepted = listener.accept() => match accepted {
                        Ok(accepted) => accepted,
                        Err(_) => continue,
                    },
                };
                let connection = state.connections.fetch_add(1, Ordering::Relaxed) + 1;
                let (acceptor, state, stop) = (acceptor.clone(), state.clone(), stop.clone());
                tokio::spawn(async move {
                    let Ok(tls) = acceptor.accept(tcp).await else {
                        return;
                    };
                    let service = hyper::service::service_fn(move |request| {
                        answer(state.clone(), connection, request)
                    });
                    let serving = hyper::server::conn::http1::Builder::new()
                        .serve_connection(TokioIo::new(tls), service);
                    tokio::select! {
                        _ = stop.cancelled() => {}
                        _ = serving => {}
                    }
                });
            }
        });
        *self.up.lock().unwrap() = Some(token);
    }

    /// Stop serving: connects are refused, connections closed.
    pub fn go_down(&self) {
        if let Some(token) = self.up.lock().unwrap().take() {
            token.cancel();
        }
        // The accept loop drops its listener as it sees the cancel.
        for _ in 0..50 {
            if std::net::TcpListener::bind(self.addr).is_ok() {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!(
            "controller stub: {} still bound after going down",
            self.addr
        );
    }

    pub fn set(&self, mode: Mode) {
        self.state.mode.send_replace(mode);
    }

    pub fn requests(&self) -> Vec<Recorded> {
        self.state.requests.lock().unwrap().clone()
    }

    pub fn answers(&self) -> Vec<Answered> {
        self.state.answers.lock().unwrap().clone()
    }

    /// The first request, from the `from`th on, that `matches`, waiting up
    /// to `timeout` for it.
    pub fn wait_for(
        &self,
        from: usize,
        timeout: Duration,
        matches: impl Fn(&Recorded) -> bool,
    ) -> Option<Recorded> {
        let deadline = Instant::now() + timeout;
        loop {
            let found = self.requests().into_iter().skip(from).find(|r| matches(r));
            if found.is_some() || Instant::now() > deadline {
                return found;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for ControllerStub {
    fn drop(&mut self) {
        if let Some(token) = self.up.lock().unwrap().take() {
            token.cancel();
        }
    }
}

async fn answer(
    state: Arc<State>,
    connection: u64,
    request: Request<Incoming>,
) -> Result<Response<Full<Bytes>>, Infallible> {
    let path = request.uri().path().to_string();
    let header = |name: &str| {
        request
            .headers()
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
    };
    let authorization = header(hyper::header::AUTHORIZATION.as_str());
    let token = header(portail::sync::TOKEN_HEADER);
    let body = match request.into_body().collect().await {
        Ok(body) => body.to_bytes(),
        Err(_) => {
            return Ok(reply(
                &state,
                connection,
                StatusCode::BAD_REQUEST,
                None,
                b"",
            ))
        }
    };
    let Ok(sync) = serde_json::from_slice::<SyncRequest>(&body) else {
        return Ok(reply(
            &state,
            connection,
            StatusCode::BAD_REQUEST,
            None,
            b"",
        ));
    };
    // The controller's contract: the path names the body's Gateway.
    let own = format!("/v1/sync/{}/{}", sync.gateway.namespace, sync.gateway.name);
    if path != own {
        return Ok(reply(
            &state,
            connection,
            StatusCode::BAD_REQUEST,
            None,
            b"path",
        ));
    }
    let have = sync.have.clone();
    let nonce = sync.nonce.clone();
    let gateway = sync.gateway.clone();
    let leaving = sync.report.as_ref().is_some_and(|report| report.leaving);
    state.requests.lock().unwrap().push(Recorded {
        at: Instant::now(),
        path,
        connection,
        authorization,
        token,
        request: sync,
    });
    // The controller signs every 200, 204 and 404 over the poll's nonce.
    let sign_as =
        |response, key: &CertifiedKey, kind, body: &[u8]| signed(response, key, kind, &nonce, body);
    // As the controller: "unchanged" over the version served, "pending"
    // with none, "leave" to a leaving poll.
    let no_content = |kind, over: &[u8]| {
        let response = reply(&state, connection, StatusCode::NO_CONTENT, None, b"");
        sign_as(response, &state.signer, kind, over)
    };

    if leaving {
        return Ok(no_content(Signed::Leave, b""));
    }
    let mut mode = state.mode.subscribe();
    let deadline = tokio::time::Instant::now() + state.hold;
    loop {
        let current = mode.borrow_and_update().clone();
        match &current {
            Mode::Gone => {
                let gone = serde_json::to_vec(&Gone { gateway }).unwrap();
                let response = reply(&state, connection, StatusCode::NOT_FOUND, None, &gone);
                return Ok(sign_as(response, &state.signer, Signed::Gone, &gone));
            }
            Mode::Unavailable(after) => {
                let status = StatusCode::SERVICE_UNAVAILABLE;
                return Ok(reply(&state, connection, status, *after, b"standby"));
            }
            Mode::Serve(Some(snapshot)) if snapshot.version != have => {
                let json = serde_json::to_vec(&snapshot).unwrap();
                let response = reply(&state, connection, StatusCode::OK, None, &json);
                let response = sign_as(response, &state.signer, Signed::Snapshot, &json);
                let mut first = state.first_snapshot.lock().unwrap();
                if first.is_none() {
                    *first = Some((response.headers().clone(), json));
                }
                return Ok(response);
            }
            Mode::Replay => {
                let recorded = state.first_snapshot.lock().unwrap().clone();
                if let Some((headers, json)) = recorded {
                    let mut response = reply(&state, connection, StatusCode::OK, None, &json);
                    *response.headers_mut() = headers;
                    return Ok(response);
                }
            }
            Mode::Forged(snapshot) if snapshot.version != have => {
                let json = serde_json::to_vec(&snapshot).unwrap();
                let response = reply(&state, connection, StatusCode::OK, None, &json);
                return Ok(sign_as(response, &state.forger, Signed::Snapshot, &json));
            }
            Mode::Tampered { signed, sent } if sent.version != have => {
                let json = serde_json::to_vec(&sent).unwrap();
                let original = serde_json::to_vec(&signed).unwrap();
                let response = reply(&state, connection, StatusCode::OK, None, &json);
                return Ok(sign_as(
                    response,
                    &state.signer,
                    Signed::Snapshot,
                    &original,
                ));
            }
            Mode::Serve(_) | Mode::Forged(_) | Mode::Tampered { .. } => {}
        }
        tokio::select! {
            _ = mode.changed() => {}
            _ = tokio::time::sleep_until(deadline) => {
                return Ok(match current {
                    Mode::Serve(None) => no_content(Signed::Pending, b""),
                    _ => no_content(Signed::Unchanged, have.as_bytes()),
                });
            }
        }
    }
}

fn reply(
    state: &State,
    connection: u64,
    status: StatusCode,
    retry_after: Option<u64>,
    body: &[u8],
) -> Response<Full<Bytes>> {
    state.answers.lock().unwrap().push(Answered {
        at: Instant::now(),
        connection,
        status,
    });
    let mut response = Response::builder().status(status);
    if let Some(seconds) = retry_after {
        response = response.header(hyper::header::RETRY_AFTER, seconds);
    }
    response
        .body(Full::new(Bytes::copy_from_slice(body)))
        .unwrap()
}

/// `response` with the signature of `body` by `key`, over `nonce`.
fn signed(
    mut response: Response<Full<Bytes>>,
    key: &CertifiedKey,
    kind: Signed,
    nonce: &str,
    body: &[u8],
) -> Response<Full<Bytes>> {
    for (name, value) in sign(key, kind, nonce, body).unwrap() {
        response.headers_mut().insert(name, value);
    }
    response
}

/// A CA's PEM, and a server config whose certificate it signed for `ip`
/// and [`CONTROLLER_NAME`], with that certificate's key.
fn test_pki(ip: std::net::IpAddr) -> (String, rustls::ServerConfig, CertifiedKey) {
    use rcgen::{BasicConstraints, CertificateParams, CertifiedIssuer, IsCa, KeyPair};
    let mut ca_params = CertificateParams::new(Vec::<String>::new()).unwrap();
    ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    let ca = CertifiedIssuer::self_signed(ca_params, KeyPair::generate().unwrap()).unwrap();
    let leaf_key = KeyPair::generate().unwrap();
    let leaf = CertificateParams::new(vec![ip.to_string(), CONTROLLER_NAME.to_string()])
        .unwrap()
        .signed_by(&leaf_key, &ca)
        .unwrap();
    let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(leaf_key.serialize_der()));
    let chain: Vec<CertificateDer<'static>> = vec![leaf.der().clone()];
    let provider = rustls::crypto::aws_lc_rs::default_provider();
    let signer = CertifiedKey::from_der(chain.clone(), key.clone_key(), &provider).unwrap();
    let mut tls = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(chain, key)
        .unwrap();
    tls.alpn_protocols = vec![b"http/1.1".to_vec()];
    (ca.pem(), tls, signer)
}

/// The snapshot of `config` (a `PortailConfig` in JSON) for [`GATEWAY`].
pub fn snapshot(config: serde_json::Value) -> Snapshot {
    let config: PortailConfig = serde_json::from_value(config).unwrap();
    let (namespace, name) = GATEWAY.split_once('/').unwrap();
    let gateway = GatewayRef {
        namespace: namespace.into(),
        name: name.into(),
        uid: "gw-uid".into(),
        generation: 1,
    };
    Snapshot::from_config(gateway, config, vec![])
}

/// One HTTP listener on `port` routing everything to `backend`.
pub fn http_snapshot(port: u16, backend: SocketAddr) -> Snapshot {
    snapshot(serde_json::json!({
        "gateway": {"name": "gw", "listeners": [
            {"name": "http", "protocol": "HTTP", "port": port},
        ]},
        "httpRoutes": [{
            "parentRefs": [{"name": "gw", "sectionName": "http"}],
            "rules": [{
                "matches": [{"path": {"type": "PathPrefix", "value": "/"}}],
                "backendRefs": [{"name": backend.ip().to_string(), "port": backend.port()}],
            }],
        }],
    }))
}

/// A port free on every interface for TCP and UDP, below the ephemeral
/// range: the kernel hands ephemeral ports to client sockets, one of which
/// could take it between this probe and the data plane's bind.
pub fn free_port() -> u16 {
    use std::hash::BuildHasher;
    let range = std::fs::read_to_string("/proc/sys/net/ipv4/ip_local_port_range").unwrap();
    let first_ephemeral: u16 = range.split_whitespace().next().unwrap().parse().unwrap();
    let unprivileged = 1024..first_ephemeral;
    let len = unprivileged.len();
    // Tests running beside this one start elsewhere.
    let start = std::hash::RandomState::new().hash_one(()) as usize;
    (0..len)
        .map(|i| unprivileged.start + ((start + i) % len) as u16)
        .find(|&port| {
            std::net::TcpListener::bind(("0.0.0.0", port)).is_ok()
                && std::net::UdpSocket::bind(("0.0.0.0", port)).is_ok()
        })
        .expect("a free port below the ephemeral range")
}
