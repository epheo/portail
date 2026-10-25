//! `portail controller` as a process, against a fake apiserver: it
//! bootstraps its CA and lease, turns ready once its caches listed, serves
//! /v1/sync over TLS from that CA, and on SIGTERM releases the lease and
//! exits 0.
//!
//! Run with: cargo test --release --test test_controller

mod helpers;

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use serde_json::{json, Value};

use helpers::{extract_status, http_request, PortailProcess};

const NS: &str = "portail-system";
const AUDIENCE: &str = "portail-controller.portail-system.svc";
const ADMIN_PORT: u16 = 25191;
const SYNC_PORT: u16 = 25192;
const CA_MAP: &str = "/api/v1/namespaces/portail-system/configmaps/portail-controller-ca";
const LEASE: &str =
    "/apis/coordination.k8s.io/v1/namespaces/portail-system/leases/portail-controller";

/// An apiserver over plain HTTP: every collection lists empty and watches
/// stay open, a streaming list ends its initial events at once, the
/// controller's own objects are created and updated in memory, and
/// TokenReview authenticates every token as `default/portail-gw`'s pod.
#[derive(Clone, Default)]
struct FakeApiserver {
    objects: Arc<Mutex<HashMap<String, Value>>>,
    requests: Arc<Mutex<Vec<(String, String)>>>,
}

impl FakeApiserver {
    fn spawn() -> (Self, SocketAddr) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let api = FakeApiserver::default();
        let server = api.clone();
        thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let server = server.clone();
                thread::spawn(move || server.serve(stream));
            }
        });
        (api, addr)
    }

    fn object(&self, path: &str) -> Option<Value> {
        self.objects.lock().unwrap().get(path).cloned()
    }

    fn requested(&self, method: &str, prefix: &str) -> usize {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .filter(|(m, p)| m == method && p.starts_with(prefix))
            .count()
    }

    fn serve(&self, mut stream: TcpStream) {
        let Some((method, target, body)) = read_request(&mut stream) else {
            return;
        };
        let (path, query) = target.split_once('?').unwrap_or((&target, ""));
        self.requests
            .lock()
            .unwrap()
            .push((method.clone(), path.to_string()));
        if query
            .split('&')
            .any(|p| p == "watch=true" || p == "watch=1")
        {
            return watch(stream, query.contains("sendInitialEvents=true"));
        }
        let (code, reply) = self.answer(&method, path, body);
        let body = serde_json::to_vec(&reply).unwrap();
        let head = format!(
            "HTTP/1.1 {code} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        let _ = stream.write_all(head.as_bytes());
        let _ = stream.write_all(&body);
    }

    fn answer(&self, method: &str, path: &str, body: Value) -> (u16, Value) {
        let not_found = || {
            (
                404,
                json!({"kind": "Status", "apiVersion": "v1", "status": "Failure",
                       "reason": "NotFound", "message": "not found", "code": 404}),
            )
        };
        let mut objects = self.objects.lock().unwrap();
        match method {
            "POST" if path.ends_with("/tokenreviews") => (
                201,
                json!({
                    "apiVersion": "authentication.k8s.io/v1", "kind": "TokenReview", "spec": {},
                    "status": {
                        "authenticated": true,
                        "audiences": [AUDIENCE],
                        "user": {
                            "username": "system:serviceaccount:default:portail-gw",
                            "extra": {"authentication.kubernetes.io/pod-uid": ["pod-1"]},
                        },
                    },
                }),
            ),
            "POST" => {
                let mut created = body;
                created["metadata"]["resourceVersion"] = json!("1");
                let item = format!("{path}/{}", created["metadata"]["name"].as_str().unwrap());
                objects.insert(item, created.clone());
                (201, created)
            }
            "PUT" | "PATCH" => match objects.get_mut(path) {
                Some(object) => {
                    if method == "PUT" {
                        *object = body;
                    }
                    let version: u64 = object["metadata"]["resourceVersion"]
                        .as_str()
                        .and_then(|v| v.parse().ok())
                        .unwrap_or(1);
                    object["metadata"]["resourceVersion"] = json!((version + 1).to_string());
                    (200, object.clone())
                }
                None => not_found(),
            },
            "GET" => match objects.get(path) {
                Some(object) => (200, object.clone()),
                // The controller's own objects, and API discovery of the
                // optional CRDs: absent.
                None if path.ends_with("/portail-controller-ca")
                    || path.ends_with("/portail-controller")
                    || path.matches('/').count() == 3 =>
                {
                    not_found()
                }
                None => (
                    200,
                    json!({"apiVersion": "v1", "kind": "List",
                           "metadata": {"resourceVersion": "1"}, "items": []}),
                ),
            },
            _ => not_found(),
        }
    }
}

/// The request line's method and target, and the JSON body.
fn read_request(stream: &mut TcpStream) -> Option<(String, String, Value)> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    let head_end = loop {
        if let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break end + 4;
        }
        let n = stream.read(&mut chunk).ok().filter(|&n| n > 0)?;
        buf.extend_from_slice(&chunk[..n]);
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
    let mut words = head.split_whitespace();
    let (method, target) = (words.next()?.to_string(), words.next()?.to_string());
    let length = head
        .lines()
        .find_map(|l| {
            let (name, value) = l.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().ok())?
        })
        .unwrap_or(0);
    while buf.len() < head_end + length {
        let n = stream.read(&mut chunk).ok().filter(|&n| n > 0)?;
        buf.extend_from_slice(&chunk[..n]);
    }
    let body = serde_json::from_slice(&buf[head_end..head_end + length]).unwrap_or(Value::Null);
    Some((method, target, body))
}

/// A watch with no events, open until the client hangs up; a streaming
/// list first says its initial events are over.
fn watch(mut stream: TcpStream, initial_events: bool) {
    let _ = stream.write_all(
        b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\n\r\n",
    );
    if initial_events {
        let event = json!({"type": "BOOKMARK", "object": {
            "apiVersion": "v1", "kind": "Secret",
            "metadata": {"resourceVersion": "1",
                         "annotations": {"k8s.io/initial-events-end": "true"}},
        }})
        .to_string()
            + "\n";
        let _ = stream.write_all(format!("{:x}\r\n{event}\r\n", event.len()).as_bytes());
    }
    let mut sink = [0u8; 1024];
    while matches!(stream.read(&mut sink), Ok(n) if n > 0) {}
}

/// Poll `check` every 50ms until it holds or `timeout` passes.
fn eventually(timeout: Duration, mut check: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if check() {
            return true;
        }
        thread::sleep(Duration::from_millis(50));
    }
    false
}

fn admin_status(path: &str) -> Option<u16> {
    let admin = SocketAddr::from(([127, 0, 0, 1], ADMIN_PORT));
    let request = format!("GET {path} HTTP/1.1\r\nHost: x\r\n\r\n");
    extract_status(&http_request(admin, request.as_bytes()).ok()?)
}

/// A projected token's claims for Gateway `default/gw`'s data plane.
fn token() -> String {
    let part = |v: Value| URL_SAFE_NO_PAD.encode(serde_json::to_vec(&v).unwrap());
    let exp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        + 3600;
    format!(
        "{}.{}.{}",
        part(json!({"alg": "RS256"})),
        part(
            json!({"aud": [AUDIENCE], "sub": "system:serviceaccount:default:portail-gw", "exp": exp})
        ),
        URL_SAFE_NO_PAD.encode(b"signature")
    )
}

/// POST /v1/sync/default/gw over TLS, trusting only `ca_pem` for the controller
/// Service's name; the status code.
fn sync(ca_pem: &str, token: &str) -> u16 {
    let mut roots = rustls::RootCertStore::empty();
    for cert in
        <rustls::pki_types::CertificateDer as rustls::pki_types::pem::PemObject>::pem_slice_iter(
            ca_pem.as_bytes(),
        )
    {
        roots.add(cert.unwrap()).unwrap();
    }
    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let name = rustls::pki_types::ServerName::try_from(AUDIENCE).unwrap();
    let conn = rustls::ClientConnection::new(Arc::new(config), name).unwrap();
    let sock = TcpStream::connect(SocketAddr::from(([127, 0, 0, 1], SYNC_PORT))).unwrap();
    sock.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let mut tls = rustls::StreamOwned::new(conn, sock);
    let body =
        json!({"protocol": 1, "gateway": {"namespace": "default", "name": "gw"}}).to_string();
    let request = format!(
        "POST /v1/sync/default/gw HTTP/1.1\r\nHost: {AUDIENCE}\r\nAuthorization: Bearer {token}\r\n\
         Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    tls.write_all(request.as_bytes()).unwrap();
    let mut response = Vec::new();
    let mut chunk = [0u8; 4096];
    while !response.windows(2).any(|w| w == b"\r\n") {
        match tls.read(&mut chunk) {
            Ok(n) if n > 0 => response.extend_from_slice(&chunk[..n]),
            other => panic!("no response: {other:?}"),
        }
    }
    extract_status(&response).unwrap()
}

#[test]
fn the_controller_serves_from_its_ca_and_releases_its_lease_on_sigterm() {
    let (api, addr) = FakeApiserver::spawn();
    let mut controller = PortailProcess::start_kubernetes(
        &[
            "controller",
            "--namespace",
            NS,
            "--sync-port",
            &SYNC_PORT.to_string(),
            "--admin-address",
            "127.0.0.1",
        ],
        Some(addr),
        ADMIN_PORT,
    );
    assert!(
        eventually(Duration::from_secs(20), || admin_status("/readyz")
            == Some(200)),
        "never ready:\n{}",
        controller.output()
    );
    assert_eq!(admin_status("/livez"), Some(200));

    let ca = api.object(CA_MAP).expect("CA published")["data"]["ca.crt"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(eventually(Duration::from_secs(5), || api
        .object(LEASE)
        .is_some_and(|l| l["spec"]["holderIdentity"]
            .as_str()
            .is_some_and(|h| !h.is_empty()))));

    assert_eq!(sync(&ca, "junk"), 401);
    assert_eq!(api.requested("POST", "/apis/authentication.k8s.io"), 0);
    // Authenticated, for a Gateway the store does not hold.
    assert_eq!(sync(&ca, &token()), 404);
    assert_eq!(api.requested("POST", "/apis/authentication.k8s.io"), 1);

    controller.terminate();
    let status = controller
        .wait_exit(Duration::from_secs(15))
        .unwrap_or_else(|| panic!("still running after SIGTERM:\n{}", controller.output()));
    assert!(status.success(), "{status}:\n{}", controller.output());
    assert_eq!(
        api.object(LEASE).unwrap()["spec"]["holderIdentity"],
        "",
        "released for a standby"
    );
}

#[test]
fn a_controller_without_a_cluster_exits_non_zero() {
    let mut controller =
        PortailProcess::start_kubernetes(&["controller", "--namespace", NS], None, 25193);
    let status = controller
        .wait_exit(Duration::from_secs(10))
        .unwrap_or_else(|| panic!("still running:\n{}", controller.output()));
    assert_eq!(status.code(), Some(1), "{}", controller.output());
}
