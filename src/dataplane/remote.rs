//! The data plane's end of `POST /v1/sync`: one keep-alive HTTPS
//! connection, to the controller or through the apiserver's service proxy
//! (`super::apiserver_proxy`), answers taken only when signed by the
//! controller, and the backoff between polls that failed.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use anyhow::Context;
use bytes::Bytes;
use http::header::{HeaderName, AUTHORIZATION, CONTENT_LENGTH, CONTENT_TYPE, HOST, RETRY_AFTER};
use http::{HeaderMap, HeaderValue, Request, Response, StatusCode, Uri};
use http_body_util::{BodyExt, Full, LengthLimitError, Limited};
use hyper::body::Incoming;
use hyper::client::conn::http1::SendRequest;
use hyper_util::rt::TokioIo;
use rustls::pki_types::ServerName;
use rustls::RootCertStore;
use tokio::net::TcpStream;
use tokio::task::JoinHandle;
use tokio_rustls::TlsConnector;

use crate::cli::CONTROLLER_CA_ENV;
use crate::sync::{
    ca_roots, new_nonce, AnswerVerifier, GatewayKey, Gone, Signed, SyncRequest, HOLD,
    STANDBY_HEADER, TOKEN_HEADER,
};

/// Bounds a connect, TCP and TLS together: in-cluster connects take
/// milliseconds, so this only cuts short a dead endpoint during failover.
/// The leave report on SIGTERM is bounded by it too.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);

/// A held poll is answered within the hold; one unanswered for twice it
/// went to a peer that is gone.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(2 * HOLD.as_secs());

/// Appended to the controller address's own path, then the Gateway's
/// namespace and name: the controller checks the token against the path
/// before it reads the body.
const SYNC_PATH: &str = "/v1/sync";

/// The longest answer body read. Gateway API allows 64 listeners of 64
/// certificateRefs: 4096 certificates at 8 KiB of PEM chain and key is
/// 32 MiB; twice that leaves the routes room.
const MAX_ANSWER: usize = 64 * 1024 * 1024;

/// The share of an error answer's body a log line keeps: a reason, not a
/// proxy's error page.
const ERROR_BODY_LOG_BYTES: usize = 512;

/// The longest wait before polling again after a standby's 503, uniform
/// below it. That is no overload: each new connection picks again among the
/// controller Service's endpoints (about half the time the leader, with two
/// replicas), so a few quick tries find it after a failover, and the jitter
/// keeps a fleet from coming back in step.
pub const STANDBY_RETRY: Duration = Duration::from_secs(1);

/// The first backoff ceiling: the rendered startupProbe period, so a data
/// plane started beside a controller still coming up retries within it.
const FIRST_BACKOFF: Duration = Duration::from_secs(1);

/// How a poll ended.
#[derive(Debug)]
pub enum Answer {
    /// 200: a snapshot, as sent, signed by the controller.
    Snapshot(Bytes),
    /// 204, signed by the controller over this poll's `have`: the version
    /// served is current.
    Unchanged,
    /// 204, signed "pending": the controller has no snapshot for this
    /// Gateway yet. It answered, and confirms nothing.
    Pending,
    /// 204 to a leaving poll, signed "leave": the controller took it.
    Left,
    /// 404, signed by the controller, recent and for this Gateway: it
    /// serves this Gateway nothing.
    Gone,
    /// 503 from a replica that serves nothing: a standby, one still
    /// syncing, or one stopping. Another replica may, on a new connection.
    Standby(String),
    /// A 200, 204 or 404 not signed by the controller for this poll, or a
    /// gone answer for another Gateway: whoever sent it, nothing it says is
    /// served.
    Unverified(String),
    /// Anything else, a failed connect or request included: try again
    /// later, after `after` when the controller said so.
    Retry {
        after: Option<Duration>,
        error: String,
    },
}

/// The controller's sync endpoint, as a data plane polls it.
pub struct Client {
    hop: Hop,
    via: Via,
    gateway: GatewayKey,
    answers: AnswerVerifier,
    /// Idle between polls; taken by the poll that uses it.
    conn: Option<Conn>,
    /// The longest answer body read, [`MAX_ANSWER`] but in tests.
    max_answer: usize,
}

/// Where a poll is sent, and the TLS peer it must reach.
pub(super) struct Hop {
    /// Dialed by name each connect: a Service whose address moved is
    /// followed.
    pub host: String,
    pub port: u16,
    /// The `Host` header.
    pub authority: String,
    pub path: String,
    pub server_name: ServerName<'static>,
    pub tls: TlsConnector,
}

impl Hop {
    /// A hop to `target`, whose certificate must chain to `roots` for
    /// `server_name`, in one of TLS `versions`.
    pub fn new(
        target: Target,
        server_name: ServerName<'static>,
        roots: RootCertStore,
        versions: &[&'static rustls::SupportedProtocolVersion],
    ) -> Self {
        let mut tls = rustls::ClientConfig::builder_with_protocol_versions(versions)
            .with_root_certificates(roots)
            .with_no_client_auth();
        tls.alpn_protocols = vec![b"http/1.1".to_vec()];
        Self {
            host: target.host,
            port: target.port,
            authority: target.authority,
            path: target.path,
            server_name,
            tls: TlsConnector::from(Arc::new(tls)),
        }
    }
}

/// How a poll authenticates. Token files are read per request: kubelet
/// rotates them.
pub(super) enum Via {
    /// To the controller: its token as the bearer.
    Direct { token_file: PathBuf },
    /// Through the apiserver: the API-audience token as the bearer, which
    /// the apiserver checks and drops, and the controller's in
    /// X-Portail-Token, which it passes on.
    ApiserverProxy {
        api_token_file: PathBuf,
        token_file: PathBuf,
    },
}

impl Via {
    async fn headers(&self) -> Result<Vec<(HeaderName, HeaderValue)>, String> {
        match self {
            Via::Direct { token_file } => Ok(vec![(AUTHORIZATION, bearer(token_file).await?)]),
            Via::ApiserverProxy {
                api_token_file,
                token_file,
            } => Ok(vec![
                (AUTHORIZATION, bearer(api_token_file).await?),
                (
                    HeaderName::from_static(TOKEN_HEADER),
                    header_value(&read_token(token_file).await?, token_file)?,
                ),
            ]),
        }
    }

    /// Whether a standby's answer drops the connection, so that the next
    /// one may reach another replica. The apiserver picks an endpoint per
    /// request: its connection is kept.
    fn redials_on_standby(&self) -> bool {
        matches!(self, Via::Direct { .. })
    }

    /// What the apiserver itself answered, not the controller.
    fn apiserver_refusal(&self, status: StatusCode, body: &[u8]) -> Option<String> {
        match self {
            Via::Direct { .. } => None,
            Via::ApiserverProxy { .. } => super::apiserver_proxy::refusal(status, body),
        }
    }
}

/// One connection to the controller. Dropping it closes it, a request in
/// flight on it too: a poll cancelled mid-hold would otherwise keep its
/// connection busy until the hold ends.
struct Conn {
    sender: SendRequest<Full<Bytes>>,
    task: JoinHandle<()>,
}

impl Drop for Conn {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// The controller CA's PEM: `file`, else `env`, the value of
/// [`CONTROLLER_CA_ENV`].
pub fn controller_ca(file: Option<&Path>, env: Option<String>) -> anyhow::Result<Vec<u8>> {
    match (file, env) {
        (Some(path), _) => {
            std::fs::read(path).with_context(|| format!("--controller-ca-file {}", path.display()))
        }
        (None, Some(pem)) if !pem.trim().is_empty() => Ok(pem.into_bytes()),
        (None, _) => anyhow::bail!(
            "no controller CA: pass --controller-ca-file or set {}",
            CONTROLLER_CA_ENV
        ),
    }
}

impl Client {
    /// A client of `gateway`'s sync endpoint under `address` (an https
    /// URL), which must present a certificate for its host chaining to
    /// `ca_pem`, and sign its answers with it.
    pub fn new(
        address: &str,
        gateway: &GatewayKey,
        ca_pem: &[u8],
        token_file: PathBuf,
    ) -> anyhow::Result<Self> {
        let target = Target::parse(address, gateway)
            .with_context(|| format!("--controller-address {:?}", address))?;
        let roots = ca_roots(ca_pem, "controller CA")?;
        let server_name = ServerName::try_from(target.host.clone())
            .with_context(|| format!("--controller-address {:?}: host", address))?;
        let answers = AnswerVerifier::new(roots.clone(), &target.host)?;
        Ok(Self::with(
            // Both ends are ours: TLS 1.3 only.
            Hop::new(target, server_name, roots, &[&rustls::version::TLS13]),
            Via::Direct { token_file },
            gateway.clone(),
            answers,
        ))
    }

    pub(super) fn with(hop: Hop, via: Via, gateway: GatewayKey, answers: AnswerVerifier) -> Self {
        Self {
            hop,
            via,
            gateway,
            answers,
            conn: None,
            max_answer: MAX_ANSWER,
        }
    }

    /// One poll, bounded by twice the hold. Its token is read from the
    /// token file now: kubelet rotates it.
    pub async fn poll(&mut self, request: &SyncRequest) -> Answer {
        match tokio::time::timeout(REQUEST_TIMEOUT, self.exchange(request)).await {
            Ok(Ok(answer)) => answer,
            Ok(Err(error)) => Answer::Retry { after: None, error },
            Err(_) => Answer::Retry {
                after: None,
                error: format!("no answer within {:?}", REQUEST_TIMEOUT),
            },
        }
    }

    async fn exchange(&mut self, request: &SyncRequest) -> Result<Answer, String> {
        let credentials = self.via.headers().await?;
        let nonce = new_nonce();
        let request = SyncRequest {
            nonce: nonce.clone(),
            ..request.clone()
        };
        let body = Bytes::from(serde_json::to_vec(&request).map_err(|e| e.to_string())?);
        let leaving = request.report.as_ref().is_some_and(|r| r.leaving);
        let have = request.have;
        let request = self.post(credentials, body);
        let response = match self.conn.take() {
            Some(mut conn) if !conn.sender.is_closed() => match send(&mut conn, request).await {
                Ok(response) => (conn, response),
                // The controller closed the idle connection before the poll
                // was written: once more on a new one. A poll written and
                // lost (a controller that crashed or rolled) goes through
                // the backoff instead, or a fleet would reconnect in step.
                Err(Failed {
                    unsent: Some(request),
                    ..
                }) => self.send_new(*request).await?,
                Err(Failed { error, .. }) => {
                    return Err(format!("request to {}: {}", self.hop.authority, error))
                }
            },
            _ => self.send_new(request).await?,
        };
        let (conn, response) = response;
        let (head, body) = response.into_parts();
        let status = head.status;
        let standby = head.headers.contains_key(STANDBY_HEADER);
        // Read before it is verified: whoever answers must not make it
        // hold more than the largest snapshot.
        let declared = head
            .headers
            .get(CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<u64>().ok());
        if let Some(len) = declared.filter(|len| *len > self.max_answer as u64) {
            return Ok(Answer::Unverified(format!(
                "an answer of {} bytes, over {}",
                len, self.max_answer
            )));
        }
        let body = match Limited::new(body, self.max_answer).collect().await {
            Ok(body) => body.to_bytes(),
            Err(e) if e.downcast_ref::<LengthLimitError>().is_some() => {
                return Ok(Answer::Unverified(format!(
                    "an answer over {} bytes",
                    self.max_answer
                )))
            }
            Err(e) => return Err(format!("reading the answer: {}", e)),
        };
        let answer = match status {
            StatusCode::OK => match self.verify(Signed::Snapshot, &nonce, &head.headers, &body) {
                Ok(()) => Answer::Snapshot(body),
                Err(e) => return Ok(Answer::Unverified(format!("snapshot refused: {}", e))),
            },
            StatusCode::NO_CONTENT => {
                let as_kind = |kind, over: &[u8]| self.verify(kind, &nonce, &head.headers, over);
                if leaving {
                    match as_kind(Signed::Leave, b"") {
                        Ok(()) => Answer::Left,
                        Err(e) => return Ok(Answer::Unverified(format!("204 refused: {}", e))),
                    }
                } else {
                    match as_kind(Signed::Unchanged, have.as_bytes()) {
                        Ok(()) => Answer::Unchanged,
                        Err(_) if as_kind(Signed::Pending, b"").is_ok() => Answer::Pending,
                        // A leave acknowledgement lands here too: never a
                        // confirmation of what a poll serves.
                        Err(e) => return Ok(Answer::Unverified(format!("204 refused: {}", e))),
                    }
                }
            }
            StatusCode::SERVICE_UNAVAILABLE if standby => {
                if !self.via.redials_on_standby() {
                    self.conn = Some(conn);
                }
                return Ok(Answer::Standby(format!(
                    "controller replica not serving: {}",
                    excerpt(&body)
                )));
            }
            status => {
                if let Some(error) = self.via.apiserver_refusal(status, &body) {
                    self.conn = Some(conn);
                    return Ok(Answer::Retry {
                        after: retry_after(&head.headers),
                        error,
                    });
                }
                if status != StatusCode::NOT_FOUND {
                    return Ok(Answer::Retry {
                        after: retry_after(&head.headers),
                        error: format!("controller answered {}: {}", status, excerpt(&body)),
                    });
                }
                match self.gone(&nonce, &head.headers, &body) {
                    Ok(()) => Answer::Gone,
                    Err(e) => return Ok(Answer::Unverified(format!("404 refused: {}", e))),
                }
            }
        };
        self.conn = Some(conn);
        Ok(answer)
    }

    /// Whether a 404 is the controller's word that this Gateway is gone.
    fn gone(&self, nonce: &str, headers: &HeaderMap, body: &[u8]) -> Result<(), String> {
        self.verify(Signed::Gone, nonce, headers, body)?;
        Gone::check(body, &self.gateway)
    }

    fn verify(
        &self,
        kind: Signed,
        nonce: &str,
        headers: &HeaderMap,
        body: &[u8],
    ) -> Result<(), String> {
        self.answers
            .verify(kind, nonce, headers, body, SystemTime::now())
    }

    async fn send_new(
        &self,
        request: Request<Full<Bytes>>,
    ) -> Result<(Conn, Response<Incoming>), String> {
        let mut conn = self.connect().await?;
        let response = send(&mut conn, request)
            .await
            .map_err(|failed| format!("request to {}: {}", self.hop.authority, failed.error))?;
        Ok((conn, response))
    }

    fn post(
        &self,
        credentials: Vec<(HeaderName, HeaderValue)>,
        body: Bytes,
    ) -> Request<Full<Bytes>> {
        let mut request = Request::post(self.hop.path.as_str())
            .header(HOST, self.hop.authority.as_str())
            .header(CONTENT_TYPE, "application/json");
        for (name, value) in credentials {
            request = request.header(name, value);
        }
        request
            .body(Full::new(body))
            .expect("the path and authority parsed from a URI, the tokens as header values")
    }

    async fn connect(&self) -> Result<Conn, String> {
        let hop = &self.hop;
        let connecting = async {
            let tcp = TcpStream::connect((hop.host.as_str(), hop.port))
                .await
                .map_err(|e| format!("connect to {}: {}", hop.authority, e))?;
            let _ = tcp.set_nodelay(true);
            let tls = hop
                .tls
                .connect(hop.server_name.clone(), tcp)
                .await
                .map_err(|e| format!("TLS with {}: {}", hop.authority, e))?;
            let (sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(tls))
                .await
                .map_err(|e| format!("HTTP with {}: {}", hop.authority, e))?;
            let task = tokio::spawn(async move {
                let _ = connection.await;
            });
            Ok(Conn { sender, task })
        };
        tokio::time::timeout(CONNECT_TIMEOUT, connecting)
            .await
            .unwrap_or_else(|_| {
                Err(format!(
                    "connect to {}: no connection within {:?}",
                    hop.authority, CONNECT_TIMEOUT
                ))
            })
    }
}

/// A request that got no answer; `unsent` when it was never written.
struct Failed {
    unsent: Option<Box<Request<Full<Bytes>>>>,
    error: hyper::Error,
}

async fn send(
    conn: &mut Conn,
    request: Request<Full<Bytes>>,
) -> Result<Response<Incoming>, Failed> {
    if let Err(error) = conn.sender.ready().await {
        return Err(Failed {
            unsent: Some(Box::new(request)),
            error,
        });
    }
    conn.sender
        .try_send_request(request)
        .await
        .map_err(|mut failed| Failed {
            unsent: failed.take_message().map(Box::new),
            error: failed.into_error(),
        })
}

/// Where an https URL points, its path followed by the Gateway's sync path.
#[derive(Debug, PartialEq)]
pub(super) struct Target {
    pub host: String,
    pub port: u16,
    pub authority: String,
    pub path: String,
}

impl Target {
    pub fn parse(address: &str, gateway: &GatewayKey) -> anyhow::Result<Self> {
        let uri: Uri = address.parse()?;
        anyhow::ensure!(uri.scheme_str() == Some("https"), "not an https URL");
        anyhow::ensure!(uri.query().is_none(), "takes no query");
        let authority = uri.authority().context("no host")?;
        anyhow::ensure!(
            !authority.as_str().contains('@'),
            "takes no user information"
        );
        let host = authority.host();
        let path = format!(
            "{}{}/{}/{}",
            uri.path().trim_end_matches('/'),
            SYNC_PATH,
            gateway.namespace,
            gateway.name
        );
        // As sent, whole: no query, no fragment cut off, no byte refused.
        let whole = http::uri::PathAndQuery::try_from(path.as_str())
            .is_ok_and(|parsed| parsed.path() == path);
        anyhow::ensure!(whole, "Gateway {} makes no URL path", gateway);
        Ok(Self {
            host: host
                .trim_start_matches('[')
                .trim_end_matches(']')
                .to_string(),
            port: authority.port_u16().unwrap_or(443),
            authority: authority.as_str().to_string(),
            path,
        })
    }
}

/// The token in `path`, read now.
async fn read_token(path: &Path) -> Result<String, String> {
    let token = tokio::fs::read_to_string(path)
        .await
        .map_err(|e| format!("token {}: {}", path.display(), e))?;
    let token = token.trim();
    if token.is_empty() {
        return Err(format!("token {}: empty", path.display()));
    }
    Ok(token.to_string())
}

/// `value`, read from `path`, as a header value kept out of Debug.
fn header_value(value: &str, path: &Path) -> Result<HeaderValue, String> {
    let mut value = HeaderValue::from_str(value)
        .map_err(|_| format!("token {}: not a header value", path.display()))?;
    value.set_sensitive(true);
    Ok(value)
}

/// The `Authorization` value for the token in `path`, read now.
async fn bearer(path: &Path) -> Result<HeaderValue, String> {
    header_value(&format!("Bearer {}", read_token(path).await?), path)
}

/// `Retry-After` in seconds; its date form is not sent by the controller.
fn retry_after(headers: &HeaderMap) -> Option<Duration> {
    let seconds = headers
        .get(RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim()
        .parse()
        .ok()?;
    Some(Duration::from_secs(seconds))
}

pub(super) fn excerpt(body: &[u8]) -> String {
    let text = String::from_utf8_lossy(&body[..body.len().min(ERROR_BODY_LOG_BYTES)]);
    text.trim().to_string()
}

/// Full-jitter exponential backoff: each wait is uniform below a ceiling
/// that doubles from [`FIRST_BACKOFF`] up to the hold, the longest a
/// healthy controller makes a poll wait anyway.
#[derive(Debug)]
pub struct Backoff {
    ceiling: Duration,
}

impl Default for Backoff {
    fn default() -> Self {
        Self {
            ceiling: FIRST_BACKOFF,
        }
    }
}

impl Backoff {
    /// The next wait.
    pub fn wait(&mut self) -> Duration {
        self.next_ceiling().mul_f64(fastrand::f64())
    }

    /// The next wait, never shorter than `floor` (a Retry-After) capped at
    /// the hold. A Retry-After alone would bring a fleet back in step
    /// after a controller rollout, and a controller stuck answering 503
    /// would be polled at its pace forever: the backoff spreads and slows
    /// them as with any failure.
    pub fn wait_at_least(&mut self, floor: Option<Duration>) -> Duration {
        let jittered = self.wait();
        floor.map_or(jittered, |floor| floor.min(HOLD).max(jittered))
    }

    /// The next ceiling itself, for retries that need no jitter.
    pub fn next_ceiling(&mut self) -> Duration {
        let ceiling = self.ceiling;
        self.ceiling = (ceiling * 2).min(HOLD);
        ceiling
    }

    pub fn reset(&mut self) {
        self.ceiling = FIRST_BACKOFF;
    }
}

/// A TLS server for tests, answering as the controller or the apiserver
/// would.
#[cfg(test)]
pub(super) mod stub {
    use std::net::SocketAddr;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use bytes::Bytes;
    use http::{HeaderMap, Request, Response};
    use http_body_util::{BodyExt, Full};
    use hyper::body::Incoming;
    use hyper_util::rt::TokioIo;
    use rustls::server::{ClientHello, ResolvesServerCert};
    use rustls::sign::CertifiedKey;

    /// A request as the stub saw it.
    #[derive(Clone, Debug)]
    pub struct Seen {
        /// Which connection, from 1.
        pub connection: usize,
        pub path: String,
        pub headers: HeaderMap,
        pub body: Bytes,
    }

    /// An answer, when it comes; `None` hangs up without one.
    pub type Reply = futures::future::BoxFuture<'static, Option<Response<Full<Bytes>>>>;

    #[derive(Debug)]
    struct Fixed(Arc<CertifiedKey>);

    impl ResolvesServerCert for Fixed {
        fn resolve(&self, _hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
            Some(self.0.clone())
        }
    }

    /// A server on 127.0.0.1 presenting `leaf`, answering each request by
    /// `answer` (its number, from 1). Returns its address and the requests
    /// seen.
    pub async fn serve(
        leaf: CertifiedKey,
        keep_alive: bool,
        answer: impl Fn(usize, Seen) -> Reply + Send + Sync + 'static,
    ) -> (SocketAddr, Arc<Mutex<Vec<Seen>>>) {
        serve_tls(leaf, keep_alive, rustls::DEFAULT_VERSIONS, answer).await
    }

    /// [`serve`], speaking only TLS `versions`.
    pub async fn serve_tls(
        leaf: CertifiedKey,
        keep_alive: bool,
        versions: &[&'static rustls::SupportedProtocolVersion],
        answer: impl Fn(usize, Seen) -> Reply + Send + Sync + 'static,
    ) -> (SocketAddr, Arc<Mutex<Vec<Seen>>>) {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        let mut tls = rustls::ServerConfig::builder_with_protocol_versions(versions)
            .with_no_client_auth()
            .with_cert_resolver(Arc::new(Fixed(Arc::new(leaf))));
        tls.alpn_protocols = vec![b"http/1.1".to_vec()];
        let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(tls));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let answer = Arc::new(answer);
        let connections = Arc::new(AtomicUsize::new(0));
        let log = seen.clone();
        tokio::spawn(async move {
            while let Ok((tcp, _)) = listener.accept().await {
                let (acceptor, answer, log) = (acceptor.clone(), answer.clone(), log.clone());
                let connection = connections.fetch_add(1, Ordering::SeqCst) + 1;
                tokio::spawn(async move {
                    let Ok(tls) = acceptor.accept(tcp).await else {
                        return;
                    };
                    let service = hyper::service::service_fn(move |request: Request<Incoming>| {
                        let (answer, log) = (answer.clone(), log.clone());
                        async move {
                            let (head, body) = request.into_parts();
                            // Read whole, as a held poll is: a hang-up
                            // before that may land while the client still
                            // writes, a poll it may then take as unsent.
                            let body = body.collect().await.map(|b| b.to_bytes());
                            let seen = Seen {
                                connection,
                                path: head.uri.path().to_string(),
                                headers: head.headers,
                                body: body.unwrap_or_default(),
                            };
                            let n = {
                                let mut log = log.lock().unwrap();
                                log.push(seen.clone());
                                log.len()
                            };
                            match answer(n, seen).await {
                                Some(response) => Ok(response),
                                None => Err(std::io::Error::other("hang up")),
                            }
                        }
                    });
                    let _ = hyper::server::conn::http1::Builder::new()
                        .keep_alive(keep_alive)
                        .serve_connection(TokioIo::new(tls), service)
                        .await;
                });
            }
        });
        (addr, seen)
    }

    /// Connections the requests in `seen` came on.
    pub fn connections(seen: &Mutex<Vec<Seen>>) -> usize {
        let seen = seen.lock().unwrap();
        let mut ids: Vec<usize> = seen.iter().map(|s| s.connection).collect();
        ids.dedup();
        ids.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sync::signed_testing::{signed, TestCa};

    #[test]
    fn backoff_starts_at_one_second_and_caps_at_the_hold() {
        let mut backoff = Backoff::default();
        let ceilings: Vec<u64> = (0..8).map(|_| backoff.next_ceiling().as_secs()).collect();
        assert_eq!(ceilings, [1, 2, 4, 8, 16, 30, 30, 30]);
        backoff.reset();
        assert_eq!(backoff.next_ceiling(), FIRST_BACKOFF);

        let mut backoff = Backoff::default();
        for ceiling in [1, 2, 4, 8, 16, 30, 30] {
            assert!(backoff.wait() <= Duration::from_secs(ceiling));
        }
    }

    /// A Retry-After is a floor, not the wait: past it the jittered
    /// backoff spreads a fleet and slows its polls as the ceiling grows.
    /// It was the wait itself, every time.
    #[test]
    fn a_retry_after_floors_the_jittered_backoff() {
        fastrand::seed(7);
        let floor = Duration::from_secs(1);
        let mut backoff = Backoff::default();
        let waits: Vec<Duration> = (0..8).map(|_| backoff.wait_at_least(Some(floor))).collect();
        assert!(waits.iter().all(|w| *w >= floor && *w <= HOLD), "{waits:?}");
        assert!(waits[3..].iter().any(|w| *w > 2 * floor), "{waits:?}");
        assert!(Backoff::default().wait_at_least(Some(3 * HOLD)) <= HOLD);
    }

    type Seen = std::sync::Arc<std::sync::Mutex<Vec<stub::Seen>>>;

    /// A one-off TLS server for 127.0.0.1 that answers each request by
    /// `answer` (its number, from 1): `None` hangs up without an answer.
    /// Returns its address, the CA PEM and the requests seen.
    async fn server(
        keep_alive: bool,
        answer: fn(usize) -> Option<StatusCode>,
    ) -> (std::net::SocketAddr, String, Seen) {
        server_with(keep_alive, answer, false).await
    }

    /// [`server`], its 503s marked as a standby's when `standby`.
    async fn server_with(
        keep_alive: bool,
        answer: fn(usize) -> Option<StatusCode>,
        standby: bool,
    ) -> (std::net::SocketAddr, String, Seen) {
        let ca = TestCa::new();
        let leaf = ca.leaf(&["127.0.0.1"]);
        let key = leaf.clone();
        let (addr, seen) = stub::serve(leaf, keep_alive, move |n, seen| {
            let response = answer(n).map(|status| {
                let mut response = Response::builder()
                    .status(status)
                    .body(Full::new(Bytes::new()))
                    .unwrap();
                if standby && status == StatusCode::SERVICE_UNAVAILABLE {
                    response
                        .headers_mut()
                        .insert(STANDBY_HEADER, HeaderValue::from_static("true"));
                }
                if status == StatusCode::NO_CONTENT {
                    let polled = polled(&seen);
                    *response.headers_mut() = signed(
                        &key,
                        Signed::Unchanged,
                        &polled.nonce,
                        polled.have.as_bytes(),
                    );
                }
                response
            });
            Box::pin(async move { response })
        })
        .await;
        (addr, ca.pem(), seen)
    }

    /// The poll a stub saw.
    fn polled(seen: &stub::Seen) -> SyncRequest {
        serde_json::from_slice(&seen.body).unwrap()
    }

    fn client(addr: std::net::SocketAddr, ca: &str, dir: &tempfile::TempDir) -> Client {
        let token = dir.path().join("token");
        std::fs::write(&token, "t").unwrap();
        let gateway = GatewayKey::new("ns", "gw");
        Client::new(&format!("https://{addr}"), &gateway, ca.as_bytes(), token).unwrap()
    }

    fn poll() -> SyncRequest {
        SyncRequest {
            protocol: crate::sync::PROTOCOL,
            gateway: GatewayKey::new("ns", "gw"),
            have: String::new(),
            data_plane_version: String::new(),
            report: None,
            nonce: String::new(),
        }
    }

    /// A standby's 503 says so, and drops the connection: the next poll
    /// connects again and may reach the leader. An overloaded replica's 503
    /// is a failure to back off from.
    #[tokio::test]
    async fn a_standby_is_told_from_an_overloaded_replica() {
        let dir = tempfile::tempdir().unwrap();
        let (addr, ca, _) =
            server_with(true, |_| Some(StatusCode::SERVICE_UNAVAILABLE), true).await;
        let mut standby = client(addr, &ca, &dir);
        let answer = standby.poll(&poll()).await;
        assert!(matches!(answer, Answer::Standby(_)), "{answer:?}");
        assert!(standby.conn.is_none(), "a new connection next time");

        let (addr, ca, _) = server(true, |_| Some(StatusCode::SERVICE_UNAVAILABLE)).await;
        let answer = client(addr, &ca, &dir).poll(&poll()).await;
        assert!(matches!(answer, Answer::Retry { .. }), "{answer:?}");
    }

    /// A poll written and lost (the controller crashed or rolled while
    /// holding it) fails to the backoff: no second request at once. Every
    /// data plane reconnected in step.
    #[tokio::test]
    async fn a_poll_lost_after_it_was_written_is_not_sent_again_at_once() {
        let (addr, ca, seen) = server(true, |n| (n == 1).then_some(StatusCode::NO_CONTENT)).await;
        let dir = tempfile::tempdir().unwrap();
        let mut client = client(addr, &ca, &dir);
        assert!(matches!(client.poll(&poll()).await, Answer::Unchanged));
        let lost = client.poll(&poll()).await;
        assert!(
            matches!(lost, Answer::Retry { after: None, .. }),
            "{lost:?}"
        );
        assert_eq!(seen.lock().unwrap().len(), 2);
    }

    /// A keep-alive the controller closed costs no failure: the poll goes
    /// on a new connection.
    #[tokio::test]
    async fn a_closed_keep_alive_is_replaced_without_a_failure() {
        let (addr, ca, seen) = server(false, |_| Some(StatusCode::NO_CONTENT)).await;
        let dir = tempfile::tempdir().unwrap();
        let mut client = client(addr, &ca, &dir);
        for _ in 0..3 {
            let answer = client.poll(&poll()).await;
            assert!(matches!(answer, Answer::Unchanged), "{answer:?}");
        }
        assert_eq!(seen.lock().unwrap().len(), 3);
    }

    /// How a test controller answers one poll.
    type Reply = Box<dyn Fn(&SyncRequest) -> Response<Full<Bytes>> + Send + Sync>;

    /// An answer of `status` with `body`, signed by `key` as `kind` over
    /// `nonce` when given.
    fn answer(
        status: StatusCode,
        body: &[u8],
        signed_as: Option<(&rustls::sign::CertifiedKey, Signed, &str)>,
    ) -> Response<Full<Bytes>> {
        let mut response = Response::builder()
            .status(status)
            .body(Full::new(Bytes::copy_from_slice(body)))
            .unwrap();
        if let Some((key, kind, nonce)) = signed_as {
            *response.headers_mut() = signed(key, kind, nonce, body);
        }
        response
    }

    fn gone_body(gateway: GatewayKey) -> Vec<u8> {
        serde_json::to_vec(&Gone { gateway }).unwrap()
    }

    /// Only what the controller signed for this poll is taken: a snapshot,
    /// a 204 over the poll's `have`, a gone answer for this Gateway.
    /// Unsigned, tampered, another CA's leaf, an answer signed for another
    /// poll (replayed), another Gateway's gone, or one kind's signature on
    /// another: each refused, and nothing served changes.
    #[tokio::test]
    async fn only_answers_the_controller_signed_for_this_poll_are_taken() {
        let ca = TestCa::new();
        let leaf = std::sync::Arc::new(ca.leaf(&["127.0.0.1"]));
        let rogue = std::sync::Arc::new(TestCa::new().leaf(&["127.0.0.1"]));
        const SNAPSHOT: &[u8] = br#"{"version":"v1"}"#;
        let own = || GatewayKey::new("ns", "gw");
        let ok = StatusCode::OK;
        let gone = StatusCode::NOT_FOUND;
        let unchanged = StatusCode::NO_CONTENT;
        let reply = |f: Reply| f;
        let (l, r) = (leaf.clone(), rogue.clone());
        let cases: Vec<(&str, Reply, fn(&Answer) -> bool)> = vec![
            (
                "signed snapshot",
                reply(Box::new({
                    let l = l.clone();
                    move |p| answer(ok, SNAPSHOT, Some((&l, Signed::Snapshot, &p.nonce)))
                })),
                |a| matches!(a, Answer::Snapshot(_)),
            ),
            (
                "unsigned snapshot",
                reply(Box::new(move |_| answer(ok, SNAPSHOT, None))),
                unverified,
            ),
            (
                "tampered snapshot",
                reply(Box::new({
                    let l = l.clone();
                    move |p| {
                        let mut response = answer(ok, br#"{"version":"v2"}"#, None);
                        *response.headers_mut() = signed(&l, Signed::Snapshot, &p.nonce, SNAPSHOT);
                        response
                    }
                })),
                unverified,
            ),
            (
                "another CA's snapshot",
                reply(Box::new(move |p| {
                    answer(ok, SNAPSHOT, Some((&r, Signed::Snapshot, &p.nonce)))
                })),
                unverified,
            ),
            (
                "a snapshot replayed from another poll",
                reply(Box::new({
                    let l = l.clone();
                    move |_| {
                        answer(
                            ok,
                            SNAPSHOT,
                            Some((&l, Signed::Snapshot, "an earlier poll")),
                        )
                    }
                })),
                unverified,
            ),
            (
                "signed 204",
                reply(Box::new({
                    let l = l.clone();
                    move |p| {
                        let mut response = answer(unchanged, b"", None);
                        *response.headers_mut() =
                            signed(&l, Signed::Unchanged, &p.nonce, p.have.as_bytes());
                        response
                    }
                })),
                |a| matches!(a, Answer::Unchanged),
            ),
            (
                "unsigned 204",
                reply(Box::new(move |_| answer(unchanged, b"", None))),
                unverified,
            ),
            (
                "a 204 over another version",
                reply(Box::new({
                    let l = l.clone();
                    move |p| {
                        let mut response = answer(unchanged, b"", None);
                        *response.headers_mut() = signed(&l, Signed::Unchanged, &p.nonce, b"v0");
                        response
                    }
                })),
                unverified,
            ),
            (
                "signed pending",
                reply(Box::new({
                    let l = l.clone();
                    move |p| {
                        let mut response = answer(unchanged, b"", None);
                        *response.headers_mut() = signed(&l, Signed::Pending, &p.nonce, b"");
                        response
                    }
                })),
                |a| matches!(a, Answer::Pending),
            ),
            (
                "a leave acknowledgement to a poll not leaving",
                reply(Box::new({
                    let l = l.clone();
                    move |p| {
                        let mut response = answer(unchanged, b"", None);
                        *response.headers_mut() = signed(&l, Signed::Leave, &p.nonce, b"");
                        response
                    }
                })),
                unverified,
            ),
            (
                "signed gone",
                reply(Box::new({
                    let l = l.clone();
                    move |p| answer(gone, &gone_body(own()), Some((&l, Signed::Gone, &p.nonce)))
                })),
                |a| matches!(a, Answer::Gone),
            ),
            (
                "a gone replayed from another poll",
                reply(Box::new({
                    let l = l.clone();
                    move |_| {
                        answer(
                            gone,
                            &gone_body(own()),
                            Some((&l, Signed::Gone, "an earlier poll")),
                        )
                    }
                })),
                unverified,
            ),
            (
                "another Gateway's gone",
                reply(Box::new({
                    let l = l.clone();
                    move |p| {
                        answer(
                            gone,
                            &gone_body(GatewayKey::new("ns", "other")),
                            Some((&l, Signed::Gone, &p.nonce)),
                        )
                    }
                })),
                unverified,
            ),
            (
                "a gone signed as a snapshot",
                reply(Box::new({
                    let l = l.clone();
                    move |p| {
                        answer(
                            gone,
                            &gone_body(own()),
                            Some((&l, Signed::Snapshot, &p.nonce)),
                        )
                    }
                })),
                unverified,
            ),
            (
                "unsigned 404",
                reply(Box::new(move |_| {
                    answer(gone, b"gateway not served\n", None)
                })),
                unverified,
            ),
        ];
        let (names, rest): (Vec<_>, Vec<_>) =
            cases.into_iter().map(|(n, f, e)| (n, (f, e))).unzip();
        let (replies, expected): (Vec<_>, Vec<_>) = rest.into_iter().unzip();
        let replies = std::sync::Arc::new(replies);
        let (addr, seen) = stub::serve((*leaf).clone(), true, {
            let replies = replies.clone();
            move |n, seen| {
                let response = replies[n - 1](&polled(&seen));
                Box::pin(async move { Some(response) })
            }
        })
        .await;
        let dir = tempfile::tempdir().unwrap();
        let mut client = client(addr, &ca.pem(), &dir);
        let request = SyncRequest {
            have: "v1".into(),
            ..poll()
        };
        for (name, expect) in names.into_iter().zip(expected) {
            let got = client.poll(&request).await;
            assert!(expect(&got), "{name}: {got:?}");
        }
        let nonces: std::collections::HashSet<String> = seen
            .lock()
            .unwrap()
            .iter()
            .map(|s| polled(s).nonce)
            .collect();
        assert_eq!(nonces.len(), replies.len(), "a fresh nonce per poll");
    }

    /// An answer bigger than the largest snapshot is refused unread: by
    /// its Content-Length, or once a chunked body passes the cap.
    #[tokio::test]
    async fn an_oversized_answer_is_refused_before_it_is_read() {
        let ca = TestCa::new();
        let (addr, _) = stub::serve(ca.leaf(&["127.0.0.1"]), true, |n, _| {
            let body = Full::new(Bytes::from(vec![b' '; 2048]));
            let mut response = Response::new(body);
            if n == 2 {
                response.headers_mut().insert(
                    http::header::TRANSFER_ENCODING,
                    HeaderValue::from_static("chunked"),
                );
            }
            Box::pin(async move { Some(response) })
        })
        .await;
        let dir = tempfile::tempdir().unwrap();
        let mut client = client(addr, &ca.pem(), &dir);
        client.max_answer = 1024;
        let declared = client.poll(&poll()).await;
        assert!(
            matches!(&declared, Answer::Unverified(e) if e.contains("2048 bytes")),
            "{declared:?}"
        );
        let streamed = client.poll(&poll()).await;
        assert!(
            matches!(&streamed, Answer::Unverified(e) if e == "an answer over 1024 bytes"),
            "{streamed:?}"
        );
    }

    /// TLS 1.2 is not spoken: a controller offering only it is no
    /// controller of this build.
    #[tokio::test]
    async fn a_peer_without_tls_1_3_is_refused() {
        let ca = TestCa::new();
        let (addr, seen) = stub::serve_tls(
            ca.leaf(&["127.0.0.1"]),
            true,
            &[&rustls::version::TLS12],
            |_, _| Box::pin(async { None }),
        )
        .await;
        let dir = tempfile::tempdir().unwrap();
        match client(addr, &ca.pem(), &dir).poll(&poll()).await {
            Answer::Retry { error, .. } => assert!(error.contains("TLS"), "{error}"),
            other => panic!("{other:?}"),
        }
        assert!(seen.lock().unwrap().is_empty());
    }

    /// A leaving poll takes a leave acknowledgement and nothing else: an
    /// "unchanged" to it would be one minted for another poll.
    #[tokio::test]
    async fn a_leaving_poll_takes_only_a_leave_acknowledgement() {
        let ca = TestCa::new();
        let leaf = ca.leaf(&["127.0.0.1"]);
        let key = leaf.clone();
        let (addr, _) = stub::serve(leaf, true, move |n, seen| {
            let polled = polled(&seen);
            let mut response = answer(StatusCode::NO_CONTENT, b"", None);
            *response.headers_mut() = match n {
                1 => signed(&key, Signed::Leave, &polled.nonce, b""),
                _ => signed(
                    &key,
                    Signed::Unchanged,
                    &polled.nonce,
                    polled.have.as_bytes(),
                ),
            };
            Box::pin(async move { Some(response) })
        })
        .await;
        let dir = tempfile::tempdir().unwrap();
        let mut client = client(addr, &ca.pem(), &dir);
        let leaving = SyncRequest {
            have: "v1".into(),
            report: Some(crate::sync::InstanceReport {
                applied: "v1".into(),
                leaving: true,
                ..Default::default()
            }),
            ..poll()
        };
        let got = client.poll(&leaving).await;
        assert!(matches!(got, Answer::Left), "{got:?}");
        let got = client.poll(&leaving).await;
        assert!(unverified(&got), "{got:?}");
    }

    fn unverified(answer: &Answer) -> bool {
        matches!(answer, Answer::Unverified(_))
    }

    #[test]
    fn the_address_names_host_port_and_path() {
        let gateway = GatewayKey::new("portail-system", "public");
        let parse = |address: &str| Target::parse(address, &gateway);
        let target = parse("https://portail-controller.portail-system.svc:9443").unwrap();
        assert_eq!(
            target,
            Target {
                host: "portail-controller.portail-system.svc".into(),
                port: 9443,
                authority: "portail-controller.portail-system.svc:9443".into(),
                path: "/v1/sync/portail-system/public".into(),
            }
        );

        // Under a path, as through the apiserver's service proxy.
        let target = parse(
            "https://apiserver.example/api/v1/namespaces/ps/services/https:portail-controller:9443/proxy/",
        )
        .unwrap();
        assert_eq!(
            (target.port, target.authority.as_str()),
            (443, "apiserver.example")
        );
        assert_eq!(
            target.path,
            "/api/v1/namespaces/ps/services/https:portail-controller:9443/proxy/v1/sync/portail-system/public"
        );

        let target = parse("https://[fd00::1]:9443").unwrap();
        assert_eq!(
            (target.host.as_str(), target.authority.as_str()),
            ("fd00::1", "[fd00::1]:9443")
        );

        for bad in [
            "http://portail-controller:9443",
            "portail-controller:9443",
            "https://portail-controller:9443/?a=b",
            "https://user@portail-controller:9443",
            "https://",
        ] {
            assert!(parse(bad).is_err(), "{bad}");
        }
    }

    /// A dotted Gateway name is a path segment as it is; a key no path can
    /// carry is refused at start, not at the first poll.
    #[test]
    fn the_path_names_the_gateway() {
        let address = "https://ctl:9443";
        let dotted = GatewayKey::new("ns", "gw.example");
        assert_eq!(
            Target::parse(address, &dotted).unwrap().path,
            "/v1/sync/ns/gw.example"
        );
        for name in ["a?b", "a#b", "a b"] {
            let key = GatewayKey::new("ns", name);
            assert!(Target::parse(address, &key).is_err(), "{name:?}");
        }
    }

    #[test]
    fn retry_after_reads_seconds() {
        let mut headers = HeaderMap::new();
        assert_eq!(retry_after(&headers), None);
        headers.insert(RETRY_AFTER, HeaderValue::from_static("3"));
        assert_eq!(retry_after(&headers), Some(Duration::from_secs(3)));
        headers.insert(
            RETRY_AFTER,
            HeaderValue::from_static("Fri, 31 Dec 1999 23:59:59 GMT"),
        );
        assert_eq!(retry_after(&headers), None);
    }

    #[tokio::test]
    async fn the_token_is_read_trimmed_and_kept_out_of_debug() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("token");
        assert!(bearer(&path).await.is_err(), "no file");
        std::fs::write(&path, "\n").unwrap();
        assert!(bearer(&path).await.is_err(), "empty");
        std::fs::write(&path, "abc.def.ghi\n").unwrap();
        let value = bearer(&path).await.unwrap();
        assert_eq!(value, "Bearer abc.def.ghi");
        assert!(value.is_sensitive());
        std::fs::write(&path, "a\nb").unwrap();
        assert!(bearer(&path).await.is_err(), "not one header line");
    }

    #[test]
    fn the_ca_comes_from_the_file_else_the_env() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ca.crt");
        std::fs::write(&path, "from file").unwrap();
        let env = || Some("from env".to_string());
        assert_eq!(controller_ca(Some(&path), env()).unwrap(), b"from file");
        assert_eq!(controller_ca(None, env()).unwrap(), b"from env");
        assert!(controller_ca(Some(&dir.path().join("missing")), env()).is_err());
        for unset in [None, Some(String::new()), Some("\n".to_string())] {
            assert!(controller_ca(None, unset).is_err());
        }
    }

    #[test]
    fn a_ca_without_a_certificate_is_refused() {
        let gateway = GatewayKey::new("ns", "gw");
        let error = Client::new("https://ctl:9443", &gateway, b"no pem", PathBuf::from("/t"))
            .err()
            .unwrap();
        assert!(error.to_string().contains("no certificate"), "{error:#}");
    }
}
