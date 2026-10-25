//! The data plane's end of `POST /v1/sync`: one keep-alive HTTPS
//! connection to the controller, trusted through the controller CA alone,
//! and the backoff between polls that failed.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use bytes::Bytes;
use http::header::{AUTHORIZATION, CONTENT_TYPE, HOST, RETRY_AFTER};
use http::{HeaderMap, HeaderValue, Request, Response, StatusCode, Uri};
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::client::conn::http1::SendRequest;
use hyper_util::rt::TokioIo;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, ServerName};
use tokio::net::TcpStream;
use tokio::task::JoinHandle;
use tokio_rustls::TlsConnector;

use crate::cli::CONTROLLER_CA_ENV;
use crate::sync::{GatewayKey, SyncRequest, HOLD};

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

/// The share of an error answer's body a log line keeps: a reason, not a
/// proxy's error page.
const ERROR_BODY_LOG_BYTES: usize = 512;

/// The first backoff ceiling: the rendered startupProbe period, so a data
/// plane started beside a controller still coming up retries within it.
const FIRST_BACKOFF: Duration = Duration::from_secs(1);

/// How a poll ended.
#[derive(Debug)]
pub enum Answer {
    /// 200: a snapshot, as sent.
    Snapshot(Bytes),
    /// 204: the version served is current.
    Unchanged,
    /// 404: the controller serves this Gateway nothing.
    Gone,
    /// Anything else, a failed connect or request included: try again
    /// later, after `after` when the controller said so.
    Retry {
        after: Option<Duration>,
        error: String,
    },
}

/// The controller's sync endpoint, as a data plane polls it.
pub struct Client {
    /// Dialed by name each connect: a Service whose address moved is
    /// followed.
    host: String,
    port: u16,
    /// The `Host` header.
    authority: String,
    path: String,
    server_name: ServerName<'static>,
    tls: TlsConnector,
    token_file: PathBuf,
    /// Idle between polls; taken by the poll that uses it.
    conn: Option<Conn>,
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
    /// `ca_pem`.
    pub fn new(
        address: &str,
        gateway: &GatewayKey,
        ca_pem: &[u8],
        token_file: PathBuf,
    ) -> anyhow::Result<Self> {
        let target = Target::parse(address, gateway)
            .with_context(|| format!("--controller-address {:?}", address))?;
        let mut roots = rustls::RootCertStore::empty();
        for cert in CertificateDer::pem_slice_iter(ca_pem) {
            roots
                .add(cert.context("controller CA")?)
                .context("controller CA")?;
        }
        anyhow::ensure!(
            !roots.is_empty(),
            "controller CA: no certificate in the PEM"
        );
        let mut tls = rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        tls.alpn_protocols = vec![b"http/1.1".to_vec()];
        let server_name = ServerName::try_from(target.host.clone())
            .with_context(|| format!("--controller-address {:?}: host", address))?;
        Ok(Self {
            host: target.host,
            port: target.port,
            authority: target.authority,
            path: target.path,
            server_name,
            tls: TlsConnector::from(Arc::new(tls)),
            token_file,
            conn: None,
        })
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
        let bearer = bearer(&self.token_file).await?;
        let body = Bytes::from(serde_json::to_vec(request).map_err(|e| e.to_string())?);
        let response = match self.conn.take() {
            Some(mut conn) if !conn.sender.is_closed() => {
                match self.send(&mut conn, &bearer, body.clone()).await {
                    Ok(response) => (conn, response),
                    // The controller may close an idle connection as the
                    // request leaves; a poll is safe to send twice.
                    Err(_) => self.send_new(&bearer, body).await?,
                }
            }
            _ => self.send_new(&bearer, body).await?,
        };
        let (conn, response) = response;
        let status = response.status();
        let after = retry_after(response.headers());
        let body = response
            .into_body()
            .collect()
            .await
            .map_err(|e| format!("reading the answer: {}", e))?
            .to_bytes();
        let answer = match status {
            StatusCode::OK => Answer::Snapshot(body),
            StatusCode::NO_CONTENT => Answer::Unchanged,
            StatusCode::NOT_FOUND => Answer::Gone,
            // A standby or a controller still syncing; dropping the
            // connection lets the Service pick a replica again.
            status => {
                return Ok(Answer::Retry {
                    after,
                    error: format!("controller answered {}: {}", status, excerpt(&body)),
                })
            }
        };
        self.conn = Some(conn);
        Ok(answer)
    }

    async fn send_new(
        &self,
        bearer: &HeaderValue,
        body: Bytes,
    ) -> Result<(Conn, Response<Incoming>), String> {
        let mut conn = self.connect().await?;
        let response = self
            .send(&mut conn, bearer, body)
            .await
            .map_err(|e| format!("request to {}: {}", self.authority, e))?;
        Ok((conn, response))
    }

    async fn send(
        &self,
        conn: &mut Conn,
        bearer: &HeaderValue,
        body: Bytes,
    ) -> Result<Response<Incoming>, hyper::Error> {
        let request = Request::post(self.path.as_str())
            .header(HOST, self.authority.as_str())
            .header(AUTHORIZATION, bearer.clone())
            .header(CONTENT_TYPE, "application/json")
            .body(Full::new(body))
            .expect("the path and authority parsed from a URI, the token as a header value");
        conn.sender.ready().await?;
        conn.sender.send_request(request).await
    }

    async fn connect(&self) -> Result<Conn, String> {
        let connecting = async {
            let tcp = TcpStream::connect((self.host.as_str(), self.port))
                .await
                .map_err(|e| format!("connect to {}: {}", self.authority, e))?;
            let _ = tcp.set_nodelay(true);
            let tls = self
                .tls
                .connect(self.server_name.clone(), tcp)
                .await
                .map_err(|e| format!("TLS with {}: {}", self.authority, e))?;
            let (sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(tls))
                .await
                .map_err(|e| format!("HTTP with {}: {}", self.authority, e))?;
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
                    self.authority, CONNECT_TIMEOUT
                ))
            })
    }
}

/// Where `--controller-address` points.
#[derive(Debug, PartialEq)]
struct Target {
    host: String,
    port: u16,
    authority: String,
    path: String,
}

impl Target {
    fn parse(address: &str, gateway: &GatewayKey) -> anyhow::Result<Self> {
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

/// The `Authorization` value for the token in `path`, read now.
async fn bearer(path: &Path) -> Result<HeaderValue, String> {
    let token = tokio::fs::read_to_string(path)
        .await
        .map_err(|e| format!("token {}: {}", path.display(), e))?;
    let token = token.trim();
    if token.is_empty() {
        return Err(format!("token {}: empty", path.display()));
    }
    let mut value = HeaderValue::from_str(&format!("Bearer {}", token))
        .map_err(|_| format!("token {}: not a header value", path.display()))?;
    value.set_sensitive(true);
    Ok(value)
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

fn excerpt(body: &[u8]) -> String {
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

#[cfg(test)]
mod tests {
    use super::*;

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
