//! An apiserver for data-plane tests: its service proxy, for one Service,
//! as the apiserver runs it. It serves as kubernetes.default.svc from its
//! own CA, takes the bearer token for its own hop and drops it, and
//! forwards the rest to a controller without verifying it.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, SignatureScheme};

/// The name every apiserver certificate carries.
const APISERVER_NAME: &str = "kubernetes.default.svc";

/// Headers that end at each hop.
const HOP_BY_HOP: [&str; 3] = ["connection", "transfer-encoding", "keep-alive"];

pub struct ApiserverStub {
    pub addr: SocketAddr,
    pub ca_pem: String,
    _runtime: tokio::runtime::Runtime,
}

struct Route {
    /// The service proxy path of the controller Service.
    prefix: String,
    bearer: String,
    controller: SocketAddr,
}

impl ApiserverStub {
    /// Proxying `prefix` (`/api/v1/namespaces/<ns>/services/https:<svc>:<port>/proxy`)
    /// to `controller` for requests bearing `token`.
    pub fn start(prefix: &str, token: &str, controller: SocketAddr) -> Self {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        let (ca_pem, tls) = pki();
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let listener = runtime
            .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
            .unwrap();
        let addr = listener.local_addr().unwrap();
        let route = Arc::new(Route {
            prefix: prefix.to_string(),
            bearer: format!("Bearer {token}"),
            controller,
        });
        let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(tls));
        runtime.spawn(async move {
            while let Ok((tcp, _)) = listener.accept().await {
                let (acceptor, route) = (acceptor.clone(), route.clone());
                tokio::spawn(async move {
                    let Ok(tls) = acceptor.accept(tcp).await else {
                        return;
                    };
                    let service =
                        hyper::service::service_fn(move |request| proxy(route.clone(), request));
                    let _ = hyper::server::conn::http1::Builder::new()
                        .serve_connection(TokioIo::new(tls), service)
                        .await;
                });
            }
        });
        Self {
            addr,
            ca_pem,
            _runtime: runtime,
        }
    }

    pub fn url(&self) -> String {
        format!("https://{}", self.addr)
    }
}

async fn proxy(
    route: Arc<Route>,
    request: Request<Incoming>,
) -> Result<Response<Full<Bytes>>, Infallible> {
    let Some(path) = request.uri().path().strip_prefix(&route.prefix) else {
        return Ok(status(
            StatusCode::NOT_FOUND,
            "the server could not find the requested resource",
        ));
    };
    let path = path.to_string();
    let authorized = request
        .headers()
        .get(hyper::header::AUTHORIZATION)
        .is_some_and(|v| v == route.bearer.as_str());
    if !authorized {
        return Ok(status(StatusCode::FORBIDDEN, "services/proxy is forbidden"));
    }
    let (head, body) = request.into_parts();
    let body = body
        .collect()
        .await
        .map(|b| b.to_bytes())
        .unwrap_or_default();
    let mut forward = Request::post(path).body(Full::new(body)).unwrap();
    for (name, value) in &head.headers {
        if name != hyper::header::AUTHORIZATION && name != hyper::header::HOST {
            forward.headers_mut().append(name, value.clone());
        }
    }
    forward.headers_mut().insert(
        hyper::header::HOST,
        route.controller.to_string().parse().unwrap(),
    );
    match forwarded(route.controller, forward).await {
        Ok(response) => Ok(response),
        Err(e) => Ok(status(
            StatusCode::SERVICE_UNAVAILABLE,
            &format!("error trying to reach service: {e}"),
        )),
    }
}

/// `request` sent to `controller` over TLS, unverified, as the apiserver
/// dials a Service; its answer relayed.
async fn forwarded(
    controller: SocketAddr,
    request: Request<Full<Bytes>>,
) -> Result<Response<Full<Bytes>>, String> {
    let tls = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(Unverified))
        .with_no_client_auth();
    let tcp = tokio::net::TcpStream::connect(controller)
        .await
        .map_err(|e| e.to_string())?;
    let name = ServerName::try_from(controller.ip().to_string()).unwrap();
    let tls = tokio_rustls::TlsConnector::from(Arc::new(tls))
        .connect(name, tcp)
        .await
        .map_err(|e| e.to_string())?;
    let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(tls))
        .await
        .map_err(|e| e.to_string())?;
    tokio::spawn(connection);
    let response = sender
        .send_request(request)
        .await
        .map_err(|e| e.to_string())?;
    let (head, body) = response.into_parts();
    let body = body.collect().await.map_err(|e| e.to_string())?.to_bytes();
    let mut relayed = Response::new(Full::new(body));
    *relayed.status_mut() = head.status;
    for (name, value) in &head.headers {
        if !HOP_BY_HOP.contains(&name.as_str()) {
            relayed.headers_mut().append(name, value.clone());
        }
    }
    Ok(relayed)
}

/// A Status object, as the apiserver refuses.
fn status(code: StatusCode, message: &str) -> Response<Full<Bytes>> {
    let body = serde_json::json!({
        "kind": "Status", "apiVersion": "v1", "metadata": {}, "status": "Failure",
        "message": message, "code": code.as_u16(),
    });
    Response::builder()
        .status(code)
        .header(hyper::header::CONTENT_TYPE, "application/json")
        .body(Full::new(Bytes::from(serde_json::to_vec(&body).unwrap())))
        .unwrap()
}

/// The apiserver's service proxy checks no certificate of the Service it
/// dials.
#[derive(Debug)]
struct Unverified;

impl ServerCertVerifier for Unverified {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        rustls::crypto::aws_lc_rs::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

/// A CA's PEM and a server config for [`APISERVER_NAME`] it signed.
fn pki() -> (String, rustls::ServerConfig) {
    use rcgen::{BasicConstraints, CertificateParams, CertifiedIssuer, IsCa, KeyPair};
    let mut ca_params = CertificateParams::new(Vec::<String>::new()).unwrap();
    ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    let ca = CertifiedIssuer::self_signed(ca_params, KeyPair::generate().unwrap()).unwrap();
    let key = KeyPair::generate().unwrap();
    let leaf = CertificateParams::new(vec![APISERVER_NAME.to_string()])
        .unwrap()
        .signed_by(&key, &ca)
        .unwrap();
    let mut tls = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(
            vec![leaf.der().clone()],
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key.serialize_der())),
        )
        .unwrap();
    tls.alpn_protocols = vec![b"http/1.1".to_vec()];
    (ca.pem(), tls)
}
