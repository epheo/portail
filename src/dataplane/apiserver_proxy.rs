//! `--transport apiserver-proxy`: the sync poll through the apiserver's
//! service proxy, for pods that reach the apiserver and nothing else on the
//! default network (OVN-Kubernetes Primary UDN).
//!
//! The apiserver authenticates the hop with the pod's API-audience token,
//! authorizes it by RBAC (`services/proxy` on the controller Service, see
//! deploy/components/apiserver-proxy), then forwards the request with its
//! body and X-Portail-Token, and holds it as long as the controller does.
//! TLS ends at the apiserver, which dials the controller without verifying
//! it: answers are taken on their signature alone (`crate::sync::signed`).

use std::path::PathBuf;

use anyhow::Context;
use http::StatusCode;
use rustls::pki_types::ServerName;

use super::remote::{excerpt, Client, Hop, Target, Via};
use crate::cli::DataplaneArgs;
use crate::sync::{ca_roots, is_dns1123_label, AnswerVerifier, GatewayKey};

/// The name every apiserver serving certificate carries: the address
/// dialed is usually its Service IP.
const APISERVER_NAME: &str = "kubernetes.default.svc";

/// Set by kubelet in every container: the apiserver Service's address.
const HOST_ENV: &str = "KUBERNETES_SERVICE_HOST";
const PORT_ENV: &str = "KUBERNETES_SERVICE_PORT";

/// A client polling the controller `args.controller_address` names through
/// the apiserver, with the controller CA `ca_pem`.
pub fn client(args: &DataplaneArgs, ca_pem: &[u8]) -> anyhow::Result<Client> {
    let apiserver = apiserver_address(
        args.apiserver_address.as_deref(),
        std::env::var(HOST_ENV).ok(),
        std::env::var(PORT_ENV).ok(),
    )?;
    let apiserver_ca = std::fs::read(&args.apiserver_ca_file)
        .with_context(|| format!("--apiserver-ca-file {}", args.apiserver_ca_file.display()))?;
    client_with(
        &Apiserver {
            address: apiserver,
            ca_pem: apiserver_ca,
            token_file: args.apiserver_token_file.clone(),
        },
        &args.controller_address,
        &args.gateway,
        ca_pem,
        args.token_file.clone(),
    )
}

/// The apiserver a proxied poll goes through.
pub struct Apiserver {
    /// An https URL.
    pub address: String,
    /// Its CA, the only roots it is trusted by.
    pub ca_pem: Vec<u8>,
    /// The API-audience token presented to it.
    pub token_file: PathBuf,
}

/// A client of `gateway`'s sync endpoint on the controller Service of
/// `controller_address`, through `apiserver`.
pub fn client_with(
    apiserver: &Apiserver,
    controller_address: &str,
    gateway: &GatewayKey,
    ca_pem: &[u8],
    token_file: PathBuf,
) -> anyhow::Result<Client> {
    let controller = ControllerService::parse(controller_address)
        .with_context(|| format!("--controller-address {:?}", controller_address))?;
    let target = Target::parse(
        &format!(
            "{}{}",
            apiserver.address.trim_end_matches('/'),
            controller.proxy_path()
        ),
        gateway,
    )
    .with_context(|| format!("--apiserver-address {:?}", apiserver.address))?;
    let roots = ca_roots(&apiserver.ca_pem, "--apiserver-ca-file")?;
    let server_name =
        ServerName::try_from(APISERVER_NAME).expect("the apiserver's name is a DNS name");
    // Every controller leaf names `<service>.<namespace>.svc`, whatever
    // the address said after it.
    let answers = AnswerVerifier::new(ca_roots(ca_pem, "controller CA")?, &controller.name())?;
    Ok(Client::with(
        Hop::new(target, server_name, roots),
        Via::ApiserverProxy {
            api_token_file: apiserver.token_file.clone(),
            token_file,
        },
        gateway.clone(),
        answers,
    ))
}

/// `flag`, else `https://<host>:<port>` from the in-cluster env.
fn apiserver_address(
    flag: Option<&str>,
    host: Option<String>,
    port: Option<String>,
) -> anyhow::Result<String> {
    if let Some(address) = flag {
        return Ok(address.to_string());
    }
    let host = host
        .filter(|h| !h.is_empty())
        .with_context(|| format!("no apiserver: pass --apiserver-address or set {HOST_ENV}"))?;
    let port = port.filter(|p| !p.is_empty());
    let port = port.as_deref().unwrap_or("443");
    Ok(if host.contains(':') {
        format!("https://[{host}]:{port}")
    } else {
        format!("https://{host}:{port}")
    })
}

/// The controller Service an address names: `<service>.<namespace>.svc`,
/// with any cluster domain after it.
#[derive(Debug, PartialEq)]
struct ControllerService {
    service: String,
    namespace: String,
    port: u16,
}

impl ControllerService {
    fn parse(address: &str) -> anyhow::Result<Self> {
        let uri: http::Uri = address.parse()?;
        anyhow::ensure!(uri.scheme_str() == Some("https"), "not an https URL");
        let authority = uri.authority().context("no host")?;
        let mut labels = authority.host().split('.');
        let (Some(service), Some(namespace), Some("svc")) =
            (labels.next(), labels.next(), labels.next())
        else {
            anyhow::bail!("not a Service name, <service>.<namespace>.svc");
        };
        anyhow::ensure!(
            is_dns1123_label(service) && is_dns1123_label(namespace),
            "not a Service name, <service>.<namespace>.svc"
        );
        Ok(Self {
            service: service.to_string(),
            namespace: namespace.to_string(),
            port: authority.port_u16().unwrap_or(443),
        })
    }

    /// The Service's own name, the one every controller leaf carries.
    fn name(&self) -> String {
        format!("{}.{}.svc", self.service, self.namespace)
    }

    /// The apiserver path that proxies to the Service's port over TLS; the
    /// Role of deploy/components/apiserver-proxy names `https:<service>:<port>`.
    fn proxy_path(&self) -> String {
        format!(
            "/api/v1/namespaces/{}/services/https:{}:{}/proxy",
            self.namespace, self.service, self.port
        )
    }
}

/// What to fix when the apiserver itself refused the hop: its answers are
/// Status objects, the controller's never are.
pub(super) fn refusal(status: StatusCode, body: &[u8]) -> Option<String> {
    let value: serde_json::Value = serde_json::from_slice(body).ok()?;
    if value.get("kind").and_then(serde_json::Value::as_str) != Some("Status") {
        return None;
    }
    let message = value
        .get("message")
        .and_then(serde_json::Value::as_str)
        .map_or_else(|| excerpt(body), str::to_string);
    let fix = match status {
        StatusCode::UNAUTHORIZED => "the API token was refused; check --apiserver-token-file",
        StatusCode::FORBIDDEN => {
            "configuration error: bind the Role of deploy/components/apiserver-proxy to this \
             Gateway's ServiceAccount"
        }
        StatusCode::NOT_FOUND => "no such controller Service; check --controller-address",
        StatusCode::BAD_GATEWAY | StatusCode::SERVICE_UNAVAILABLE => {
            "the controller Service has no endpoint the apiserver reached"
        }
        _ => "the apiserver refused the proxied poll",
    };
    Some(format!(
        "apiserver answered {}: {}; {}",
        status, message, fix
    ))
}

#[cfg(test)]
mod tests {
    use super::super::remote::stub::{self, Seen};
    use super::super::remote::Answer;
    use super::*;
    use crate::sync::signed_testing::{signed, TestCa};
    use crate::sync::{Signed, SyncRequest, HOLD, PROTOCOL, STANDBY_HEADER, TOKEN_HEADER};
    use bytes::Bytes;
    use http::{header, Response};
    use http_body_util::Full;
    use std::sync::Arc;
    use std::time::Duration;

    const CONTROLLER: &str = "https://portail-controller.portail-system.svc:9443";
    const CONTROLLER_NAME: &str = "portail-controller.portail-system.svc";
    const PROXY_PATH: &str = "/api/v1/namespaces/portail-system/services/https:portail-controller:9443/proxy/v1/sync/demo-primary/svc-gw";

    fn gateway() -> GatewayKey {
        GatewayKey::new("demo-primary", "svc-gw")
    }

    #[test]
    fn the_proxy_path_names_the_controller_service_and_port() {
        let parse = |address: &str| ControllerService::parse(address);
        let service = parse(CONTROLLER).unwrap();
        assert_eq!(
            service.proxy_path(),
            "/api/v1/namespaces/portail-system/services/https:portail-controller:9443/proxy"
        );
        assert_eq!(service.name(), CONTROLLER_NAME);
        let long = parse("https://ctl.ps.svc.cluster.local").unwrap();
        assert_eq!((long.name().as_str(), long.port), ("ctl.ps.svc", 443));
        for bad in [
            "http://ctl.ps.svc:9443",
            "https://ctl:9443",
            "https://ctl.ps:9443",
            "https://ctl.ps.cluster.local",
            "https://10.0.0.1:9443",
            "https://Ctl.ps.svc",
        ] {
            assert!(parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn the_apiserver_is_the_flag_else_the_in_cluster_env() {
        let env = |host: &str, port: &str| (Some(host.to_string()), Some(port.to_string()));
        let (host, port) = env("10.96.0.1", "443");
        assert_eq!(
            apiserver_address(Some("https://api.example:6443"), host.clone(), port.clone())
                .unwrap(),
            "https://api.example:6443"
        );
        assert_eq!(
            apiserver_address(None, host, port).unwrap(),
            "https://10.96.0.1:443"
        );
        let (host, port) = env("fd00::1", "6443");
        assert_eq!(
            apiserver_address(None, host, port).unwrap(),
            "https://[fd00::1]:6443"
        );
        assert_eq!(
            apiserver_address(None, Some("10.96.0.1".into()), None).unwrap(),
            "https://10.96.0.1:443"
        );
        for host in [None, Some(String::new())] {
            assert!(apiserver_address(None, host, None).is_err());
        }
    }

    /// The apiserver's own refusals say what to fix; the controller's
    /// answers, passed on, are not taken for them.
    #[test]
    fn apiserver_refusals_are_told_from_controller_answers() {
        let status = |code: u16, message: &str| {
            serde_json::to_vec(&serde_json::json!({
                "kind": "Status", "apiVersion": "v1", "status": "Failure",
                "message": message, "code": code,
            }))
            .unwrap()
        };
        let forbidden = refusal(
            StatusCode::FORBIDDEN,
            &status(
                403,
                "services \"https:portail-controller:9443\" is forbidden",
            ),
        )
        .unwrap();
        assert!(
            forbidden.contains("configuration error") && forbidden.contains("is forbidden"),
            "{forbidden}"
        );
        for code in [401, 404, 503] {
            let status_code = StatusCode::from_u16(code).unwrap();
            assert!(
                refusal(status_code, &status(code, "no")).is_some(),
                "{code}"
            );
        }
        assert_eq!(refusal(StatusCode::FORBIDDEN, b"forbidden\n"), None);
        assert_eq!(
            refusal(StatusCode::NOT_FOUND, br#"{"gateway":{},"issuedAt":1}"#),
            None
        );
    }

    /// An apiserver serving as `kubernetes.default.svc` from its own CA,
    /// before a controller of another CA that signs with `controller`.
    struct Fixture {
        apiserver_ca: TestCa,
        controller_ca: TestCa,
        dir: tempfile::TempDir,
    }

    impl Fixture {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            std::fs::write(dir.path().join("api-token"), "api-token\n").unwrap();
            std::fs::write(dir.path().join("token"), "controller-token\n").unwrap();
            Self {
                apiserver_ca: TestCa::new(),
                controller_ca: TestCa::new(),
                dir,
            }
        }

        async fn apiserver(
            &self,
            answer: impl Fn(usize, Seen) -> stub::Reply + Send + Sync + 'static,
        ) -> (Client, Arc<std::sync::Mutex<Vec<Seen>>>) {
            let leaf = self.apiserver_ca.leaf(&[APISERVER_NAME]);
            let (addr, seen) = stub::serve(leaf, true, answer).await;
            let apiserver = Apiserver {
                address: format!("https://{addr}"),
                ca_pem: self.apiserver_ca.pem().into_bytes(),
                token_file: self.dir.path().join("api-token"),
            };
            let client = client_with(
                &apiserver,
                CONTROLLER,
                &gateway(),
                self.controller_ca.pem().as_bytes(),
                self.dir.path().join("token"),
            )
            .unwrap();
            (client, seen)
        }

        /// A 200 signed by the controller.
        fn snapshot(&self) -> Response<Full<Bytes>> {
            let body = br#"{"version":"v1"}"#;
            let key = self.controller_ca.leaf(&[CONTROLLER_NAME]);
            let mut response = Response::new(Full::new(Bytes::from_static(body)));
            *response.headers_mut() = signed(&key, Signed::Snapshot, body);
            response
        }
    }

    fn request() -> SyncRequest {
        SyncRequest {
            protocol: PROTOCOL,
            gateway: gateway(),
            have: String::new(),
            data_plane_version: "test".into(),
            report: None,
        }
    }

    /// The poll goes to the proxy path of the controller Service, the API
    /// token as the bearer, the controller token in X-Portail-Token, the
    /// body as sent direct; a held answer comes back signed and is taken.
    #[tokio::test(start_paused = true)]
    async fn a_poll_through_the_apiserver_carries_both_tokens_and_holds() {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        let fixture = Arc::new(Fixture::new());
        // Longer than the hold, within the request timeout of two: the
        // apiserver held 75s on hetznet, past either.
        let held = HOLD + Duration::from_secs(15);
        let answers = fixture.clone();
        let (mut client, seen) = fixture
            .apiserver(move |_, _| {
                let answer = answers.snapshot();
                Box::pin(async move {
                    tokio::time::sleep(held).await;
                    Some(answer)
                })
            })
            .await;
        let started = tokio::time::Instant::now();
        let answer = client.poll(&request()).await;
        assert!(matches!(answer, Answer::Snapshot(_)), "{answer:?}");
        assert!(started.elapsed() >= held);

        let seen = seen.lock().unwrap();
        let poll = &seen[0];
        assert_eq!(poll.path, PROXY_PATH);
        assert_eq!(poll.headers[header::AUTHORIZATION], "Bearer api-token");
        assert_eq!(poll.headers[TOKEN_HEADER], "controller-token");
        let sent: SyncRequest = serde_json::from_slice(&poll.body).unwrap();
        assert_eq!(sent, request());
    }

    /// Through the apiserver, a snapshot signed by anyone but the
    /// controller is refused: the hop's TLS proves only the apiserver.
    #[tokio::test]
    async fn an_answer_the_controller_did_not_sign_is_refused() {
        let fixture = Fixture::new();
        let rogue = TestCa::new().leaf(&[CONTROLLER_NAME]);
        let (mut client, _) = fixture
            .apiserver(move |n, _| {
                let body = br#"{"version":"rogue"}"#;
                let mut response = Response::new(Full::new(Bytes::from_static(body)));
                if n == 1 {
                    *response.headers_mut() = signed(&rogue, Signed::Snapshot, body);
                }
                Box::pin(async move { Some(response) })
            })
            .await;
        for _ in 0..2 {
            let answer = client.poll(&request()).await;
            assert!(matches!(answer, Answer::Unverified(_)), "{answer:?}");
        }
    }

    /// An RBAC refusal is a configuration error to back off from; a
    /// standby's 503 is dialed again on the same apiserver connection, as
    /// the apiserver picks an endpoint per request.
    #[tokio::test]
    async fn apiserver_refusals_back_off_and_standbys_keep_the_connection() {
        let fixture = Fixture::new();
        let (mut client, seen) = fixture
            .apiserver(|n, _| {
                let response = if n == 1 {
                    let body = serde_json::json!({
                        "kind": "Status", "apiVersion": "v1", "status": "Failure",
                        "message": "forbidden: User \"system:serviceaccount:demo-primary:portail-svc-gw\" cannot create resource \"services/proxy\"",
                        "reason": "Forbidden", "code": 403,
                    });
                    Response::builder()
                        .status(StatusCode::FORBIDDEN)
                        .header(header::CONTENT_TYPE, "application/json")
                        .body(Full::new(Bytes::from(serde_json::to_vec(&body).unwrap())))
                        .unwrap()
                } else {
                    Response::builder()
                        .status(StatusCode::SERVICE_UNAVAILABLE)
                        .header(STANDBY_HEADER, "true")
                        .body(Full::new(Bytes::from_static(b"not the leader\n")))
                        .unwrap()
                };
                Box::pin(async move { Some(response) })
            })
            .await;
        match client.poll(&request()).await {
            Answer::Retry { error, .. } => {
                assert!(error.contains("configuration error"), "{error}")
            }
            other => panic!("{other:?}"),
        }
        for _ in 0..2 {
            let answer = client.poll(&request()).await;
            assert!(matches!(answer, Answer::Standby(_)), "{answer:?}");
        }
        assert_eq!(seen.lock().unwrap().len(), 3);
        assert_eq!(stub::connections(&seen), 1, "one apiserver connection");
    }

    #[test]
    fn an_address_that_names_no_service_is_refused_at_start() {
        let fixture = Fixture::new();
        let apiserver = Apiserver {
            address: "https://10.96.0.1:443".into(),
            ca_pem: fixture.apiserver_ca.pem().into_bytes(),
            token_file: fixture.dir.path().join("api-token"),
        };
        let ca = fixture.controller_ca.pem();
        let token = fixture.dir.path().join("token");
        assert!(client_with(
            &apiserver,
            CONTROLLER,
            &gateway(),
            ca.as_bytes(),
            token.clone()
        )
        .is_ok());
        let error = client_with(
            &apiserver,
            "https://10.0.0.5:9443",
            &gateway(),
            ca.as_bytes(),
            token,
        )
        .err()
        .unwrap();
        assert!(format!("{error:#}").contains("Service name"), "{error:#}");
    }
}
