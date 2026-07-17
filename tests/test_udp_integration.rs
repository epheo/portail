//! End-to-end UDP proxy tests: real Portail process, UDP echo backends,
//! datagrams sent through the proxy's UDP listeners.
//!
//! The echo backend replies "echo[{peer_port}]:{payload}"; the embedded port
//! is the proxy's per-session backend socket, which lets tests assert session
//! reuse (same port) and per-client isolation (distinct ports) from outside.

mod helpers;

use std::net::{SocketAddr, UdpSocket};
use std::time::Duration;

use helpers::{udp_send_recv, PortailProcess, UdpEchoBackend};

/// Own range, clear of test_proxy_integration (19000+) and TLS (19700+).
fn udp_port(offset: u16) -> u16 {
    19800 + offset
}

fn parse_echo(reply: &[u8]) -> (u16, Vec<u8>) {
    let s = std::str::from_utf8(reply).expect("utf8 reply");
    let rest = s.strip_prefix("echo[").expect("echo prefix");
    let (port, payload) = rest.split_once("]:").expect("port delimiter");
    (port.parse().expect("port number"), payload.into())
}

fn client_socket() -> UdpSocket {
    UdpSocket::bind("127.0.0.1:0").expect("bind client socket")
}

#[test]
fn test_udp_datagram_forwarded_and_reply_returned() {
    let backend = UdpEchoBackend::spawn();
    let http_port = udp_port(1);
    let listen_port = udp_port(2);
    let proxy =
        PortailProcess::spawn_with_udp(&[], http_port, &[("udp", listen_port, backend.addr)], "{}");

    let target: SocketAddr = format!("127.0.0.1:{}", listen_port).parse().unwrap();
    let socket = client_socket();
    let reply = udp_send_recv(&socket, target, b"hello", 10).expect("echo reply");
    let (_, payload) = parse_echo(&reply);
    assert_eq!(payload, b"hello");

    drop(proxy);
}

#[test]
fn test_udp_session_reused_across_datagrams() {
    let backend = UdpEchoBackend::spawn();
    let http_port = udp_port(3);
    let listen_port = udp_port(4);
    let _proxy =
        PortailProcess::spawn_with_udp(&[], http_port, &[("udp", listen_port, backend.addr)], "{}");

    let target: SocketAddr = format!("127.0.0.1:{}", listen_port).parse().unwrap();
    let socket = client_socket();

    let first = udp_send_recv(&socket, target, b"one", 10).expect("first reply");
    let (first_port, payload) = parse_echo(&first);
    assert_eq!(payload, b"one");

    for msg in [&b"two"[..], &b"three"[..]] {
        let reply = udp_send_recv(&socket, target, msg, 3).expect("follow-up reply");
        let (port, payload) = parse_echo(&reply);
        assert_eq!(payload, msg);
        assert_eq!(
            port, first_port,
            "same client socket should reuse the same backend session"
        );
    }
}

#[test]
fn test_udp_clients_get_isolated_sessions() {
    let backend = UdpEchoBackend::spawn();
    let http_port = udp_port(5);
    let listen_port = udp_port(6);
    let _proxy =
        PortailProcess::spawn_with_udp(&[], http_port, &[("udp", listen_port, backend.addr)], "{}");

    let target: SocketAddr = format!("127.0.0.1:{}", listen_port).parse().unwrap();
    let client_a = client_socket();
    let client_b = client_socket();

    let reply_a = udp_send_recv(&client_a, target, b"from-a", 10).expect("client a reply");
    let reply_b = udp_send_recv(&client_b, target, b"from-b", 10).expect("client b reply");

    let (port_a, payload_a) = parse_echo(&reply_a);
    let (port_b, payload_b) = parse_echo(&reply_b);
    assert_eq!(payload_a, b"from-a");
    assert_eq!(payload_b, b"from-b");
    assert_ne!(
        port_a, port_b,
        "each client should get its own backend session"
    );

    // Interleave: replies must keep landing on the right client.
    let reply_a2 = udp_send_recv(&client_a, target, b"again-a", 3).expect("client a second");
    let (port_a2, payload_a2) = parse_echo(&reply_a2);
    assert_eq!(payload_a2, b"again-a");
    assert_eq!(port_a2, port_a);
}

#[test]
fn test_udp_two_listeners_route_to_distinct_backends() {
    let backend_one = UdpEchoBackend::spawn();
    let backend_two = UdpEchoBackend::spawn();
    let http_port = udp_port(7);
    let port_one = udp_port(8);
    let port_two = udp_port(9);
    let _proxy = PortailProcess::spawn_with_udp(
        &[],
        http_port,
        &[
            ("udp-one", port_one, backend_one.addr),
            ("udp-two", port_two, backend_two.addr),
        ],
        "{}",
    );

    // Distinct payloads per listener; the echo proves which backend answered
    // only if each backend is reachable solely through its own listener, so
    // stop one backend and check the other listener still answers.
    let target_one: SocketAddr = format!("127.0.0.1:{}", port_one).parse().unwrap();
    let target_two: SocketAddr = format!("127.0.0.1:{}", port_two).parse().unwrap();
    let socket = client_socket();

    let reply_one = udp_send_recv(&socket, target_one, b"to-one", 10).expect("listener one reply");
    assert_eq!(parse_echo(&reply_one).1, b"to-one");

    drop(backend_one);

    let socket_two = client_socket();
    let reply_two =
        udp_send_recv(&socket_two, target_two, b"to-two", 10).expect("listener two reply");
    assert_eq!(parse_echo(&reply_two).1, b"to-two");
}

#[test]
fn test_udp_session_expiry_does_not_kill_listener() {
    let backend = UdpEchoBackend::spawn();
    let http_port = udp_port(10);
    let listen_port = udp_port(11);
    let _proxy = PortailProcess::spawn_with_udp(
        &[],
        http_port,
        &[("udp", listen_port, backend.addr)],
        r#"{"udpSessionTimeout": "1s"}"#,
    );

    let target: SocketAddr = format!("127.0.0.1:{}", listen_port).parse().unwrap();
    let socket = client_socket();

    let first = udp_send_recv(&socket, target, b"before-expiry", 10).expect("first reply");
    assert_eq!(parse_echo(&first).1, b"before-expiry");

    // Past the 1s session timeout plus the reaper period (timeout/2), so the
    // session is guaranteed collected before the next datagram arrives.
    std::thread::sleep(Duration::from_millis(2500));

    let second = udp_send_recv(&socket, target, b"after-expiry", 10).expect("post-expiry reply");
    assert_eq!(parse_echo(&second).1, b"after-expiry");
}
