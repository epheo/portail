// Compiled once per test binary; each binary uses a different subset.
#![allow(dead_code)]

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command, ExitStatus};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

/// Everything the child wrote to stdout and stderr, interleaved.
type Captured = Arc<Mutex<Vec<u8>>>;

/// Piped child output must be drained or a full pipe (64 KiB) blocks the
/// process: a tokio worker stuck in write(2) on stderr wedged the h2 e2e
/// tests, because the h2 crate's per-frame debug traces outgrow the buffer.
/// Kept rather than discarded, so a test can assert on the logs.
fn drain_child_pipes(child: &mut Child) -> (Captured, Vec<thread::JoinHandle<()>>) {
    let captured = Captured::default();
    let mut readers = Vec::new();
    let pipes: [Option<Box<dyn Read + Send>>; 2] = [
        child.stdout.take().map(|p| Box::new(p) as _),
        child.stderr.take().map(|p| Box::new(p) as _),
    ];
    for mut pipe in pipes.into_iter().flatten() {
        let captured = captured.clone();
        readers.push(thread::spawn(move || {
            let mut chunk = [0u8; 8192];
            while let Ok(n @ 1..) = pipe.read(&mut chunk) {
                captured.lock().unwrap().extend_from_slice(&chunk[..n]);
            }
        }));
    }
    (captured, readers)
}

/// kubeconfig for a cluster whose apiserver is `{server}`, with no credentials.
const KUBECONFIG: &str = "apiVersion: v1
kind: Config
clusters:
- name: test
  cluster:
    server: http://{server}
contexts:
- name: test
  context:
    cluster: test
    user: test
current-context: test
users:
- name: test
  user: {}
";

/// Minimal TCP backend that accepts connections and writes canned HTTP responses.
pub struct TestBackend {
    pub addr: SocketAddr,
    shutdown: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl TestBackend {
    /// Spawn a backend that responds to every HTTP request with `response_body`.
    /// The response includes proper Content-Length and Connection headers.
    pub fn spawn(response_body: &str) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test backend");
        let addr = listener.local_addr().unwrap();
        let shutdown = Arc::new(AtomicBool::new(false));
        let shutdown_clone = shutdown.clone();
        let body = response_body.to_string();

        listener.set_nonblocking(true).unwrap();

        let handle = thread::spawn(move || {
            while !shutdown_clone.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let body = body.clone();
                        let shutdown = shutdown_clone.clone();
                        thread::spawn(move || {
                            handle_backend_connection(stream, &body, &shutdown);
                        });
                    }
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => break,
                }
            }
        });

        Self {
            addr,
            shutdown,
            handle: Some(handle),
        }
    }

    /// Spawn a backend that sends a fixed-size binary payload (for large response tests).
    pub fn spawn_large(size: usize) -> Self {
        let body = "x".repeat(size);
        Self::spawn(&body)
    }
}

fn handle_backend_connection(mut stream: TcpStream, body: &str, shutdown: &AtomicBool) {
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok();
    stream.set_write_timeout(Some(Duration::from_secs(5))).ok();

    let mut buf = [0u8; 8192];
    loop {
        if shutdown.load(Ordering::Relaxed) {
            return;
        }

        let n = match stream.read(&mut buf) {
            Ok(0) => return,
            Ok(n) => n,
            Err(_) => return,
        };

        // Wait until we see the end of HTTP headers
        let request = &buf[..n];
        if !request.windows(4).any(|w| w == b"\r\n\r\n") {
            continue;
        }

        let keep_alive = request_is_keepalive(request);

        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: {}\r\n\r\n{}",
            body.len(),
            if keep_alive { "keep-alive" } else { "close" },
            body
        );

        if stream.write_all(response.as_bytes()).is_err() {
            return;
        }

        if !keep_alive {
            return;
        }
    }
}

fn request_is_keepalive(request: &[u8]) -> bool {
    // HTTP/1.1 defaults to keep-alive unless Connection: close
    let req_str = std::str::from_utf8(request).unwrap_or("");
    if req_str.contains("HTTP/1.0") {
        return false;
    }
    !req_str.to_lowercase().contains("connection: close")
}

impl Drop for TestBackend {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        // Connect to unblock the accept loop
        let _ = TcpStream::connect(self.addr);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

/// Keepalive backend answering `ok` to every request, except that a
/// request for `/slow` is answered only after `delay`; its arrival is
/// reported on the returned channel first, so a test can act while it is
/// in flight.
pub fn spawn_slow_backend(delay: Duration) -> (SocketAddr, std::sync::mpsc::Receiver<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind slow backend");
    let addr = listener.local_addr().unwrap();
    let (arrived, rx) = std::sync::mpsc::channel();
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { return };
            let arrived = arrived.clone();
            thread::spawn(move || {
                let mut buf = [0u8; 8192];
                let mut head = Vec::new();
                while let Ok(n @ 1..) = stream.read(&mut buf) {
                    head.extend_from_slice(&buf[..n]);
                    while let Some(end) = head.windows(4).position(|w| w == b"\r\n\r\n") {
                        let request: Vec<u8> = head.drain(..end + 4).collect();
                        if request.starts_with(b"GET /slow ") {
                            let _ = arrived.send(());
                            thread::sleep(delay);
                        }
                        let reply = b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok";
                        if stream.write_all(reply).is_err() {
                            return;
                        }
                    }
                }
            });
        }
    });
    (addr, rx)
}

/// Raw TCP echo backend — echoes bytes back without HTTP framing.
pub struct TcpEchoBackend {
    pub addr: SocketAddr,
    shutdown: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl TcpEchoBackend {
    pub fn spawn() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind echo backend");
        let addr = listener.local_addr().unwrap();
        let shutdown = Arc::new(AtomicBool::new(false));
        let shutdown_clone = shutdown.clone();

        listener.set_nonblocking(true).unwrap();

        let handle = thread::spawn(move || {
            while !shutdown_clone.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let shutdown = shutdown_clone.clone();
                        thread::spawn(move || {
                            handle_echo_connection(stream, &shutdown);
                        });
                    }
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => break,
                }
            }
        });

        Self {
            addr,
            shutdown,
            handle: Some(handle),
        }
    }
}

fn handle_echo_connection(mut stream: TcpStream, shutdown: &AtomicBool) {
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok();
    stream.set_write_timeout(Some(Duration::from_secs(5))).ok();

    let mut buf = [0u8; 8192];
    loop {
        if shutdown.load(Ordering::Relaxed) {
            return;
        }
        let n = match stream.read(&mut buf) {
            Ok(0) => return,
            Ok(n) => n,
            Err(_) => return,
        };
        if stream.write_all(&buf[..n]).is_err() {
            return;
        }
    }
}

impl Drop for TcpEchoBackend {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        let _ = TcpStream::connect(self.addr);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

/// UDP backend that echoes each datagram back as "echo[{peer_port}]:{payload}".
/// The embedded port is the proxy's per-session backend socket: equal ports
/// prove session reuse, distinct ports prove session isolation.
pub struct UdpEchoBackend {
    pub addr: SocketAddr,
    shutdown: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl UdpEchoBackend {
    pub fn spawn() -> Self {
        let socket = UdpSocket::bind("127.0.0.1:0").expect("bind udp echo backend");
        let addr = socket.local_addr().unwrap();
        let shutdown = Arc::new(AtomicBool::new(false));
        let shutdown_clone = shutdown.clone();

        // Timeout so the loop notices shutdown without a wake-up packet.
        socket
            .set_read_timeout(Some(Duration::from_millis(100)))
            .unwrap();

        let handle = thread::spawn(move || {
            let mut buf = [0u8; 65536];
            while !shutdown_clone.load(Ordering::Relaxed) {
                match socket.recv_from(&mut buf) {
                    Ok((n, peer)) => {
                        let mut reply = format!("echo[{}]:", peer.port()).into_bytes();
                        reply.extend_from_slice(&buf[..n]);
                        let _ = socket.send_to(&reply, peer);
                    }
                    Err(ref e)
                        if e.kind() == std::io::ErrorKind::WouldBlock
                            || e.kind() == std::io::ErrorKind::TimedOut => {}
                    Err(_) => break,
                }
            }
        });

        Self {
            addr,
            shutdown,
            handle: Some(handle),
        }
    }
}

impl Drop for UdpEchoBackend {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

/// Send one datagram and wait for the reply, resending on timeout.
/// Retries because UDP gives no signal while the proxy's socket is
/// still binding: early datagrams are silently dropped.
pub fn udp_send_recv(
    socket: &UdpSocket,
    proxy: SocketAddr,
    payload: &[u8],
    attempts: u32,
) -> std::io::Result<Vec<u8>> {
    socket.set_read_timeout(Some(Duration::from_millis(500)))?;
    let mut buf = [0u8; 65536];
    let mut last_err = std::io::Error::new(std::io::ErrorKind::TimedOut, "no reply");
    for _ in 0..attempts {
        socket.send_to(payload, proxy)?;
        match socket.recv_from(&mut buf) {
            Ok((n, _)) => return Ok(buf[..n].to_vec()),
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                last_err = e;
            }
            Err(e) => return Err(e),
        }
    }
    Err(last_err)
}

/// Send raw TCP data and read back the response.
pub fn tcp_roundtrip(addr: SocketAddr, data: &[u8], timeout: Duration) -> std::io::Result<Vec<u8>> {
    let mut stream = TcpStream::connect_timeout(&addr, timeout)?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;
    stream.write_all(data)?;

    let mut response = Vec::with_capacity(data.len());
    let mut buf = [0u8; 8192];
    while response.len() < data.len() {
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => response.extend_from_slice(&buf[..n]),
            Err(ref e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                break
            }
            Err(e) => return Err(e),
        }
    }
    Ok(response)
}

/// Handle to a running Portail subprocess. Kills on drop.
pub struct PortailProcess {
    child: Child,
    // The --config file, or the kubeconfig in Kubernetes mode.
    _config_file: tempfile::NamedTempFile,
    // Held so the --cert-dir outlives the process (TLS tests only).
    _cert_dir: Option<tempfile::TempDir>,
    pub proxy_addr: SocketAddr,
    output: Captured,
    readers: Vec<thread::JoinHandle<()>>,
}

impl PortailProcess {
    /// Send SIGTERM, as kubelet does to stop a pod.
    pub fn terminate(&self) {
        // SAFETY: kill(2) on our own child's pid; no memory is involved.
        unsafe { libc::kill(self.child.id() as libc::pid_t, libc::SIGTERM) };
    }

    /// Wait up to `timeout` for the process to exit. On exit, the output
    /// is complete: the pipe readers are joined.
    pub fn wait_exit(&mut self, timeout: Duration) -> Option<ExitStatus> {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            if let Some(status) = self.child.try_wait().expect("try_wait") {
                for reader in self.readers.drain(..) {
                    let _ = reader.join();
                }
                return Some(status);
            }
            if std::time::Instant::now() > deadline {
                return None;
            }
            thread::sleep(Duration::from_millis(20));
        }
    }

    /// The process logs so far (stdout and stderr).
    pub fn output(&self) -> String {
        String::from_utf8_lossy(&self.output.lock().unwrap()).into_owned()
    }

    /// Spawn in Kubernetes mode against the apiserver at `apiserver`
    /// (`None`: a kubeconfig path that does not exist, so no cluster is
    /// found). Returns at once: no port is bound without a Gateway, and
    /// the admin endpoint on `admin_port` may never come up.
    pub fn start_kubernetes(apiserver: Option<SocketAddr>, admin_port: u16) -> Self {
        let kubeconfig = tempfile::Builder::new()
            .suffix(".yaml")
            .tempfile()
            .expect("create kubeconfig");
        let kubeconfig_path = match apiserver {
            Some(server) => {
                let yaml = KUBECONFIG.replace("{server}", &server.to_string());
                std::fs::write(kubeconfig.path(), yaml).expect("write kubeconfig");
                kubeconfig.path().to_path_buf()
            }
            None => kubeconfig.path().with_extension("missing"),
        };

        let binary = cargo_bin_path();
        let mut child = unsafe {
            Command::new(&binary)
                .arg("--kubernetes")
                .arg("--readiness-port")
                .arg(admin_port.to_string())
                .env("KUBECONFIG", &kubeconfig_path)
                // Never the in-cluster config of a CI pod.
                .env_remove("KUBERNETES_SERVICE_HOST")
                .env_remove("KUBERNETES_SERVICE_PORT")
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .pre_exec(|| {
                    libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL);
                    Ok(())
                })
                .spawn()
                .unwrap_or_else(|e| panic!("Failed to spawn {:?}: {}", binary, e))
        };
        let (output, readers) = drain_child_pipes(&mut child);

        Self {
            child,
            _config_file: kubeconfig,
            _cert_dir: None,
            proxy_addr: SocketAddr::from(([127, 0, 0, 1], admin_port)),
            output,
            readers,
        }
    }

    /// Build a test config and spawn Portail.
    /// `routes` maps (hostname, path_prefix) → backend SocketAddr.
    pub fn spawn(routes: &[(&str, &str, SocketAddr)], proxy_port: u16) -> Self {
        Self::spawn_with_tcp(routes, proxy_port, &[])
    }

    /// Build a test config with both HTTP and TCP routes, then spawn Portail.
    /// `tcp_routes` maps (listener_name, proxy_port) → backend SocketAddr.
    pub fn spawn_with_tcp(
        routes: &[(&str, &str, SocketAddr)],
        proxy_port: u16,
        tcp_routes: &[(&str, u16, SocketAddr)],
    ) -> Self {
        let config = build_test_config_with_tcp(routes, proxy_port, tcp_routes);
        let config_file = tempfile::Builder::new()
            .suffix(".json")
            .tempfile()
            .expect("create temp config");
        std::fs::write(config_file.path(), config).expect("write temp config");

        let binary = cargo_bin_path();
        let mut child = unsafe {
            Command::new(&binary)
                .arg("--config")
                .arg(config_file.path())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .pre_exec(|| {
                    libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL);
                    Ok(())
                })
                .spawn()
                .unwrap_or_else(|e| panic!("Failed to spawn {:?}: {}", binary, e))
        };
        let (output, readers) = drain_child_pipes(&mut child);

        let proxy_addr: SocketAddr = format!("127.0.0.1:{}", proxy_port).parse().unwrap();

        // Collect all ports to wait on (HTTP + TCP)
        let mut wait_addrs = vec![proxy_addr];
        for (_, tcp_port, _) in tcp_routes {
            let addr: SocketAddr = format!("127.0.0.1:{}", tcp_port).parse().unwrap();
            wait_addrs.push(addr);
        }

        // Wait for proxy to start accepting connections on all ports (up to 5s)
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        for wait_addr in &wait_addrs {
            loop {
                if std::time::Instant::now() > deadline {
                    if let Ok(Some(status)) = child.try_wait() {
                        panic!(
                            "Portail exited with {} before accepting connections.",
                            status
                        );
                    }
                    panic!(
                        "Portail did not start accepting connections on {} within 5s",
                        wait_addr
                    );
                }
                if TcpStream::connect_timeout(wait_addr, Duration::from_millis(100)).is_ok() {
                    break;
                }
                thread::sleep(Duration::from_millis(50));
            }
        }

        Self {
            child,
            _config_file: config_file,
            _cert_dir: None,
            proxy_addr,
            output,
            readers,
        }
    }

    /// Build a test config with HTTP and UDP routes, then spawn Portail.
    /// `udp_routes` maps (listener_name, proxy_port) to backend SocketAddr.
    /// Readiness is probed on the HTTP port only: UDP gives no connect signal,
    /// so callers must retry their first datagram (udp_send_recv does).
    pub fn spawn_with_udp(
        routes: &[(&str, &str, SocketAddr)],
        proxy_port: u16,
        udp_routes: &[(&str, u16, SocketAddr)],
        performance_json: &str,
    ) -> Self {
        let config = build_test_config_l4(routes, proxy_port, &[], udp_routes).replace(
            r#""performance": {}"#,
            &format!(r#""performance": {}"#, performance_json),
        );
        Self::spawn_config(config, proxy_port)
    }

    /// Spawn with filtered routes (hostname, path, backend, filters_json).
    pub fn spawn_with_filters(routes: &[(&str, &str, SocketAddr, &str)], proxy_port: u16) -> Self {
        let config = build_test_config_with_filters(routes, proxy_port);
        let config_file = tempfile::Builder::new()
            .suffix(".json")
            .tempfile()
            .expect("create temp config");
        std::fs::write(config_file.path(), config).expect("write temp config");

        let binary = cargo_bin_path();
        let mut child = unsafe {
            Command::new(&binary)
                .arg("--config")
                .arg(config_file.path())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .pre_exec(|| {
                    libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL);
                    Ok(())
                })
                .spawn()
                .unwrap_or_else(|e| panic!("Failed to spawn {:?}: {}", binary, e))
        };
        let (output, readers) = drain_child_pipes(&mut child);

        let proxy_addr: SocketAddr = format!("127.0.0.1:{}", proxy_port).parse().unwrap();

        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            if std::time::Instant::now() > deadline {
                panic!("Portail did not start accepting connections within 5s");
            }
            if TcpStream::connect_timeout(&proxy_addr, Duration::from_millis(100)).is_ok() {
                break;
            }
            thread::sleep(Duration::from_millis(50));
        }

        Self {
            child,
            _config_file: config_file,
            _cert_dir: None,
            proxy_addr,
            output,
            readers,
        }
    }

    /// Spawn with a custom `performance` JSON object (e.g. tight timeouts).
    pub fn spawn_with_performance(
        routes: &[(&str, &str, SocketAddr)],
        proxy_port: u16,
        performance_json: &str,
    ) -> Self {
        let config = build_test_config_with_tcp(routes, proxy_port, &[]).replace(
            r#""performance": {}"#,
            &format!(r#""performance": {}"#, performance_json),
        );
        Self::spawn_config(config, proxy_port)
    }

    /// Spawn with the standalone admin endpoint enabled on `metrics_port`.
    /// The admin server is spawned after the listeners bind, so it gets its
    /// own connect-wait: the proxy port being up does not imply /metrics is.
    pub fn spawn_with_metrics(
        routes: &[(&str, &str, SocketAddr)],
        proxy_port: u16,
        metrics_port: u16,
    ) -> Self {
        let config = build_test_config_with_tcp(routes, proxy_port, &[]);
        let proc = Self::spawn_config_args(
            config,
            proxy_port,
            &["--metrics-port", &metrics_port.to_string()],
        );
        let metrics_addr: SocketAddr = format!("127.0.0.1:{}", metrics_port).parse().unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while TcpStream::connect_timeout(&metrics_addr, Duration::from_millis(100)).is_err() {
            assert!(
                std::time::Instant::now() < deadline,
                "admin endpoint on {} not up within 5s",
                metrics_addr
            );
            thread::sleep(Duration::from_millis(20));
        }
        proc
    }

    /// Spawn with `--access-log` writing to `log_path`.
    pub fn spawn_with_access_log(
        routes: &[(&str, &str, SocketAddr)],
        proxy_port: u16,
        log_path: &std::path::Path,
    ) -> Self {
        let config = build_test_config_with_tcp(routes, proxy_port, &[]);
        Self::spawn_config_args(
            config,
            proxy_port,
            &["--access-log", log_path.to_str().unwrap()],
        )
    }

    /// Spawn from a ready config string and wait for the proxy port.
    fn spawn_config(config: String, proxy_port: u16) -> Self {
        Self::spawn_config_args(config, proxy_port, &[])
    }

    /// Spawn with extra CLI args.
    pub fn spawn_with_args(
        routes: &[(&str, &str, SocketAddr)],
        proxy_port: u16,
        extra_args: &[&str],
    ) -> Self {
        let config = build_test_config_with_tcp(routes, proxy_port, &[]);
        Self::spawn_config_args(config, proxy_port, extra_args)
    }

    /// Spawn with at most `limit` open fds, soft and hard: portail raises
    /// only the soft limit, so this one holds.
    pub fn spawn_with_fd_limit(
        routes: &[(&str, &str, SocketAddr)],
        proxy_port: u16,
        limit: u64,
        extra_args: &[&str],
    ) -> Self {
        let config = build_test_config_with_tcp(routes, proxy_port, &[]);
        Self::spawn_config_limited(config, proxy_port, extra_args, Some(limit))
    }

    /// CPU time the process has used so far, user and system.
    pub fn cpu_time(&self) -> Duration {
        let stat = std::fs::read_to_string(format!("/proc/{}/stat", self.child.id()))
            .expect("read /proc stat");
        // Fields after the parenthesized command name, which may hold spaces.
        let fields: Vec<&str> = stat
            .rsplit_once(')')
            .unwrap()
            .1
            .split_whitespace()
            .collect();
        let ticks: u64 = fields[11].parse::<u64>().unwrap() + fields[12].parse::<u64>().unwrap();
        // SAFETY: sysconf only reads a configuration value.
        let per_second = unsafe { libc::sysconf(libc::_SC_CLK_TCK) } as u64;
        Duration::from_millis(ticks * 1000 / per_second)
    }

    fn spawn_config_args(config: String, proxy_port: u16, extra_args: &[&str]) -> Self {
        Self::spawn_config_limited(config, proxy_port, extra_args, None)
    }

    fn spawn_config_limited(
        config: String,
        proxy_port: u16,
        extra_args: &[&str],
        fd_limit: Option<u64>,
    ) -> Self {
        let config_file = tempfile::Builder::new()
            .suffix(".json")
            .tempfile()
            .expect("create temp config");
        std::fs::write(config_file.path(), config).expect("write temp config");

        let binary = cargo_bin_path();
        let mut child = unsafe {
            Command::new(&binary)
                .arg("--config")
                .arg(config_file.path())
                .args(extra_args)
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .pre_exec(move || {
                    libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL);
                    if let Some(limit) = fd_limit {
                        let lim = libc::rlimit {
                            rlim_cur: limit,
                            rlim_max: limit,
                        };
                        if libc::setrlimit(libc::RLIMIT_NOFILE, &lim) != 0 {
                            return Err(std::io::Error::last_os_error());
                        }
                    }
                    Ok(())
                })
                .spawn()
                .unwrap_or_else(|e| panic!("Failed to spawn {:?}: {}", binary, e))
        };
        let (output, readers) = drain_child_pipes(&mut child);

        let proxy_addr: SocketAddr = format!("127.0.0.1:{}", proxy_port).parse().unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            if std::time::Instant::now() > deadline {
                if let Ok(Some(status)) = child.try_wait() {
                    panic!(
                        "Portail exited with {} before accepting connections.",
                        status
                    );
                }
                panic!("Portail did not start accepting connections within 5s");
            }
            if TcpStream::connect_timeout(&proxy_addr, Duration::from_millis(100)).is_ok() {
                break;
            }
            thread::sleep(Duration::from_millis(50));
        }

        Self {
            child,
            _config_file: config_file,
            _cert_dir: None,
            proxy_addr,
            output,
            readers,
        }
    }

    /// Spawn with one HTTPS Terminate listener on `proxy_port`.
    /// `certs`: (ref_name, sni_hostname, cert_pem, key_pem), written into a
    /// temp --cert-dir as {ref_name}.crt/.key. `routes`: (hostname, backend).
    pub fn spawn_with_tls(
        certs: &[(&str, &str, &[u8], &[u8])],
        routes: &[(&str, SocketAddr)],
        proxy_port: u16,
    ) -> Self {
        Self::spawn_with_tls_args(certs, routes, proxy_port, &[])
    }

    /// [`spawn_with_tls`] plus extra CLI args (e.g. `--http2`).
    pub fn spawn_with_tls_args(
        certs: &[(&str, &str, &[u8], &[u8])],
        routes: &[(&str, SocketAddr)],
        proxy_port: u16,
        extra_args: &[&str],
    ) -> Self {
        Self::spawn_with_tls_rule(certs, routes, proxy_port, extra_args, "")
    }

    /// [`spawn_with_tls_args`] plus `rule_extra`, JSON members appended to
    /// every route rule (e.g. `, "timeouts": {"request": "1s"}`).
    pub fn spawn_with_tls_rule(
        certs: &[(&str, &str, &[u8], &[u8])],
        routes: &[(&str, SocketAddr)],
        proxy_port: u16,
        extra_args: &[&str],
        rule_extra: &str,
    ) -> Self {
        let cert_dir = tempfile::tempdir().expect("create cert dir");
        for (name, _, cert_pem, key_pem) in certs {
            std::fs::write(cert_dir.path().join(format!("{}.crt", name)), cert_pem)
                .expect("write cert");
            std::fs::write(cert_dir.path().join(format!("{}.key", name)), key_pem)
                .expect("write key");
        }

        let cert_refs: Vec<String> = certs
            .iter()
            .map(|(name, host, _, _)| format!(r#"{{"name": "{}", "hostname": "{}"}}"#, name, host))
            .collect();
        let route_objs: Vec<String> = routes
            .iter()
            .map(|(host, addr)| {
                format!(
                    r#"{{
        "parentRefs": [{{"name": "test-gateway", "sectionName": "https"}}],
        "hostnames": ["{}"],
        "rules": [{{
            "matches": [{{"path": {{"type": "PathPrefix", "value": "/"}}}}],
            "backendRefs": [{{"name": "{}", "port": {}}}]{}
        }}]
    }}"#,
                    host,
                    addr.ip(),
                    addr.port(),
                    rule_extra
                )
            })
            .collect();
        let config = format!(
            r#"{{
  "gateway": {{
    "name": "test-gateway",
    "listeners": [{{
      "name": "https", "protocol": "HTTPS", "port": {},
      "tls": {{"mode": "Terminate", "certificateRefs": [{}]}}
    }}]
  }},
  "httpRoutes": [{}],
  "observability": {{
    "logging": {{"level": "debug", "format": "pretty", "output": "stderr"}}
  }},
  "performance": {{}}
}}"#,
            proxy_port,
            cert_refs.join(", "),
            route_objs.join(", ")
        );

        let config_file = tempfile::Builder::new()
            .suffix(".json")
            .tempfile()
            .expect("create temp config");
        std::fs::write(config_file.path(), config).expect("write temp config");

        let binary = cargo_bin_path();
        let mut child = unsafe {
            Command::new(&binary)
                .arg("--config")
                .arg(config_file.path())
                .arg("--cert-dir")
                .arg(cert_dir.path())
                .args(extra_args)
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .pre_exec(|| {
                    libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL);
                    Ok(())
                })
                .spawn()
                .unwrap_or_else(|e| panic!("Failed to spawn {:?}: {}", binary, e))
        };
        let (output, readers) = drain_child_pipes(&mut child);

        let proxy_addr: SocketAddr = format!("127.0.0.1:{}", proxy_port).parse().unwrap();

        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            if std::time::Instant::now() > deadline {
                if let Ok(Some(status)) = child.try_wait() {
                    panic!(
                        "Portail exited with {} before accepting connections.",
                        status
                    );
                }
                panic!("Portail did not start accepting TLS connections within 5s");
            }
            if TcpStream::connect_timeout(&proxy_addr, Duration::from_millis(100)).is_ok() {
                break;
            }
            thread::sleep(Duration::from_millis(50));
        }

        Self {
            child,
            _config_file: config_file,
            _cert_dir: Some(cert_dir),
            proxy_addr,
            output,
            readers,
        }
    }
}

/// Generate a self-signed cert+key via the openssl CLI (mirrors the lib's
/// test_util, which integration tests cannot reach across the crate boundary).
/// `None` when openssl is unavailable — callers skip their test.
pub fn generate_test_cert(cn: &str) -> Option<(Vec<u8>, Vec<u8>)> {
    let dir = tempfile::tempdir().ok()?;
    let key_path = dir.path().join("key.pem");
    let cert_path = dir.path().join("cert.pem");
    let output = Command::new("openssl")
        .args([
            "req",
            "-x509",
            "-newkey",
            "ec",
            "-pkeyopt",
            "ec_paramgen_curve:prime256v1",
            "-nodes",
            "-keyout",
        ])
        .arg(&key_path)
        .args(["-out"])
        .arg(&cert_path)
        .args(["-days", "1", "-subj", &format!("/CN={}", cn)])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let cert_pem = std::fs::read(&cert_path).ok()?;
    let key_pem = std::fs::read(&key_path).ok()?;
    Some((cert_pem, key_pem))
}

impl Drop for PortailProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Whether something accepts connections on `addr` within `timeout`.
pub fn wait_for_listener(addr: SocketAddr, timeout: Duration) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        if TcpStream::connect_timeout(&addr, Duration::from_millis(100)).is_ok() {
            return true;
        }
        thread::sleep(Duration::from_millis(20));
    }
    false
}

/// Send a raw HTTP request and return the full response bytes.
pub fn http_request(addr: SocketAddr, request: &[u8]) -> std::io::Result<Vec<u8>> {
    http_request_timeout(addr, request, Duration::from_secs(5))
}

/// Send a raw HTTP request with a custom timeout.
pub fn http_request_timeout(
    addr: SocketAddr,
    request: &[u8],
    timeout: Duration,
) -> std::io::Result<Vec<u8>> {
    let mut stream = TcpStream::connect_timeout(&addr, timeout)?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;
    stream.write_all(request)?;

    let mut response = Vec::with_capacity(4096);
    let mut buf = [0u8; 4096];
    loop {
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => response.extend_from_slice(&buf[..n]),
            Err(ref e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                // If we already have a complete HTTP response, stop
                if response_is_complete(&response) {
                    break;
                }
                // Otherwise this is a real timeout
                return Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "read timed out waiting for complete response",
                ));
            }
            Err(e) => return Err(e),
        }
    }

    Ok(response)
}

/// Send multiple HTTP requests on the same TCP connection.
pub fn http_request_keepalive(
    addr: SocketAddr,
    requests: &[&[u8]],
) -> std::io::Result<Vec<Vec<u8>>> {
    let timeout = Duration::from_secs(5);
    let mut stream = TcpStream::connect_timeout(&addr, timeout)?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;

    let mut responses = Vec::new();

    for request in requests {
        stream.write_all(request)?;

        let mut response = Vec::with_capacity(4096);
        let mut buf = [0u8; 4096];
        loop {
            match stream.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    response.extend_from_slice(&buf[..n]);
                    if response_is_complete(&response) {
                        break;
                    }
                }
                Err(ref e)
                    if e.kind() == std::io::ErrorKind::WouldBlock
                        || e.kind() == std::io::ErrorKind::TimedOut =>
                {
                    if response_is_complete(&response) {
                        break;
                    }
                    return Err(std::io::Error::from(std::io::ErrorKind::TimedOut));
                }
                Err(e) => return Err(e),
            }
        }
        responses.push(response);
    }

    Ok(responses)
}

/// Read one response (head plus Content-Length body) off a connection
/// that stays open.
pub fn read_one_response(stream: &mut TcpStream) -> std::io::Result<Vec<u8>> {
    let mut response = Vec::new();
    let mut buf = [0u8; 4096];
    while !response_is_complete(&response) {
        match stream.read(&mut buf)? {
            0 => return Err(std::io::ErrorKind::UnexpectedEof.into()),
            n => response.extend_from_slice(&buf[..n]),
        }
    }
    Ok(response)
}

/// Check if a raw HTTP response contains complete headers + content-length body.
fn response_is_complete(data: &[u8]) -> bool {
    let header_end = match data.windows(4).position(|w| w == b"\r\n\r\n") {
        Some(pos) => pos + 4,
        None => return false,
    };

    let headers = std::str::from_utf8(&data[..header_end]).unwrap_or("");
    if let Some(cl_line) = headers
        .lines()
        .find(|l| l.to_lowercase().starts_with("content-length:"))
    {
        if let Ok(cl) = cl_line
            .split(':')
            .nth(1)
            .unwrap_or("")
            .trim()
            .parse::<usize>()
        {
            return data.len() >= header_end + cl;
        }
    }

    // No content-length — assume complete if we have headers
    true
}

/// Extract the response body from a raw HTTP response.
pub fn extract_body(response: &[u8]) -> &[u8] {
    match response.windows(4).position(|w| w == b"\r\n\r\n") {
        Some(pos) => &response[pos + 4..],
        None => &[],
    }
}

/// Extract the HTTP status code from a raw response.
pub fn extract_status(response: &[u8]) -> Option<u16> {
    let first_line = response.split(|&b| b == b'\r').next()?;
    let status_str = std::str::from_utf8(first_line).ok()?;
    // "HTTP/1.1 200 OK" → "200"
    status_str.split_whitespace().nth(1)?.parse().ok()
}

fn build_test_config_with_tcp(
    routes: &[(&str, &str, SocketAddr)],
    proxy_port: u16,
    tcp_routes: &[(&str, u16, SocketAddr)],
) -> String {
    build_test_config_l4(routes, proxy_port, tcp_routes, &[])
}

fn build_test_config_l4(
    routes: &[(&str, &str, SocketAddr)],
    proxy_port: u16,
    tcp_routes: &[(&str, u16, SocketAddr)],
    udp_routes: &[(&str, u16, SocketAddr)],
) -> String {
    let mut backend_refs = Vec::new();
    let mut hostnames: Vec<String> = Vec::new();

    for (hostname, path, backend_addr) in routes {
        if !hostnames.contains(&hostname.to_string()) {
            hostnames.push(hostname.to_string());
        }
        backend_refs.push(format!(
            r#"{{
                "matches": [{{"path": {{"type": "PathPrefix", "value": "{}"}}}}],
                "backendRefs": [{{"name": "{}", "port": {}}}]
            }}"#,
            path,
            backend_addr.ip(),
            backend_addr.port()
        ));
    }

    let hostnames_json: Vec<String> = hostnames.iter().map(|h| format!("\"{}\"", h)).collect();

    // Build TCP listener entries and route entries
    let mut tcp_listener_entries = Vec::new();
    let mut tcp_route_entries = Vec::new();
    for (name, port, backend_addr) in tcp_routes {
        tcp_listener_entries.push(format!(
            r#"{{"name": "{}", "protocol": "TCP", "port": {}}}"#,
            name, port
        ));
        tcp_route_entries.push(format!(
            r#"{{
      "parentRefs": [{{"name": "test-gateway", "sectionName": "{}"}}],
      "rules": [{{"backendRefs": [{{"name": "{}", "port": {}}}]}}]
    }}"#,
            name,
            backend_addr.ip(),
            backend_addr.port()
        ));
    }

    let mut udp_listener_entries = Vec::new();
    let mut udp_route_entries = Vec::new();
    for (name, port, backend_addr) in udp_routes {
        udp_listener_entries.push(format!(
            r#"{{"name": "{}", "protocol": "UDP", "port": {}}}"#,
            name, port
        ));
        udp_route_entries.push(format!(
            r#"{{
      "parentRefs": [{{"name": "test-gateway", "sectionName": "{}"}}],
      "rules": [{{"backendRefs": [{{"name": "{}", "port": {}}}]}}]
    }}"#,
            name,
            backend_addr.ip(),
            backend_addr.port()
        ));
    }

    let mut listeners = format!(
        r#"{{"name": "http", "protocol": "HTTP", "port": {}}}"#,
        proxy_port
    );
    for entry in tcp_listener_entries.iter().chain(&udp_listener_entries) {
        listeners.push_str(", ");
        listeners.push_str(entry);
    }

    let http_routes_section = if routes.is_empty() {
        "[]".to_string()
    } else {
        format!(
            r#"[{{
      "parentRefs": [{{"name": "test-gateway", "sectionName": "http"}}],
      "hostnames": [{}],
      "rules": [{}]
    }}]"#,
            hostnames_json.join(", "),
            backend_refs.join(", ")
        )
    };

    format!(
        r#"{{
  "gateway": {{
    "name": "test-gateway",
    "listeners": [{}]
  }},
  "httpRoutes": {},
  "tcpRoutes": [{}],
  "udpRoutes": [{}],
  "observability": {{
    "logging": {{"level": "debug", "format": "pretty", "output": "stderr"}}
  }},
  "performance": {{}}
}}"#,
        listeners,
        http_routes_section,
        tcp_route_entries.join(", "),
        udp_route_entries.join(", ")
    )
}

/// Backend that records the raw request bytes received, for verifying rewrites and header modifications.
pub struct InspectingBackend {
    pub addr: SocketAddr,
    shutdown: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
    received: Arc<std::sync::Mutex<Vec<Vec<u8>>>>,
}

impl InspectingBackend {
    pub fn spawn(response_body: &str) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind inspecting backend");
        let addr = listener.local_addr().unwrap();
        let shutdown = Arc::new(AtomicBool::new(false));
        let shutdown_clone = shutdown.clone();
        let body = response_body.to_string();
        let received = Arc::new(std::sync::Mutex::new(Vec::new()));
        let received_clone = received.clone();

        listener.set_nonblocking(true).unwrap();

        let handle = thread::spawn(move || {
            while !shutdown_clone.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let body = body.clone();
                        let shutdown = shutdown_clone.clone();
                        let received = received_clone.clone();
                        thread::spawn(move || {
                            handle_inspecting_connection(stream, &body, &shutdown, &received);
                        });
                    }
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => break,
                }
            }
        });

        Self {
            addr,
            shutdown,
            handle: Some(handle),
            received,
        }
    }

    pub fn received_requests(&self) -> Vec<Vec<u8>> {
        self.received.lock().unwrap().clone()
    }
}

fn handle_inspecting_connection(
    mut stream: TcpStream,
    body: &str,
    shutdown: &AtomicBool,
    received: &std::sync::Mutex<Vec<Vec<u8>>>,
) {
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok();
    stream.set_write_timeout(Some(Duration::from_secs(5))).ok();

    let mut buf = [0u8; 8192];
    let mut accum = Vec::new();
    loop {
        if shutdown.load(Ordering::Relaxed) {
            return;
        }

        let n = match stream.read(&mut buf) {
            Ok(0) => return,
            Ok(n) => n,
            Err(_) => return,
        };

        accum.extend_from_slice(&buf[..n]);

        // Find end of headers
        let header_end = match accum.windows(4).position(|w| w == b"\r\n\r\n") {
            Some(pos) => pos + 4,
            None => continue,
        };

        // Parse Content-Length to know how much body to expect
        let headers_str = std::str::from_utf8(&accum[..header_end]).unwrap_or("");
        let content_length: usize = headers_str
            .lines()
            .find(|l| l.to_lowercase().starts_with("content-length:"))
            .and_then(|l| l.split(':').nth(1))
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(0);

        let total_expected = header_end + content_length;

        // Keep reading until we have the full body
        while accum.len() < total_expected {
            let n = match stream.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => n,
                Err(_) => break,
            };
            accum.extend_from_slice(&buf[..n]);
        }

        received.lock().unwrap().push(accum.clone());

        let keep_alive = request_is_keepalive(&accum);

        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: {}\r\n\r\n{}",
            body.len(),
            if keep_alive { "keep-alive" } else { "close" },
            body
        );

        accum.clear();

        if stream.write_all(response.as_bytes()).is_err() {
            return;
        }
        if !keep_alive {
            return;
        }
    }
}

impl Drop for InspectingBackend {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        let _ = TcpStream::connect(self.addr);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

/// Build a test config with filters support on HTTP routes.
pub fn build_test_config_with_filters(
    routes: &[(&str, &str, SocketAddr, &str)], // (hostname, path, backend, filters_json)
    proxy_port: u16,
) -> String {
    let mut backend_refs = Vec::new();
    let mut hostnames: Vec<String> = Vec::new();

    for (hostname, path, backend_addr, filters_json) in routes {
        if !hostnames.contains(&hostname.to_string()) {
            hostnames.push(hostname.to_string());
        }
        let filters_part = if filters_json.is_empty() {
            String::new()
        } else {
            format!(r#""filters": [{}],"#, filters_json)
        };
        backend_refs.push(format!(
            r#"{{
                "matches": [{{"path": {{"type": "PathPrefix", "value": "{}"}}}}],
                {}
                "backendRefs": [{{"name": "{}", "port": {}}}]
            }}"#,
            path,
            filters_part,
            backend_addr.ip(),
            backend_addr.port()
        ));
    }

    let hostnames_json: Vec<String> = hostnames.iter().map(|h| format!("\"{}\"", h)).collect();

    format!(
        r#"{{
  "gateway": {{
    "name": "test-gateway",
    "listeners": [{{"name": "http", "protocol": "HTTP", "port": {}}}]
  }},
  "httpRoutes": [{{
    "parentRefs": [{{"name": "test-gateway", "sectionName": "http"}}],
    "hostnames": [{}],
    "rules": [{}]
  }}],
  "observability": {{
    "logging": {{"level": "debug", "format": "pretty", "output": "stderr"}}
  }},
  "performance": {{}}
}}"#,
        proxy_port,
        hostnames_json.join(", "),
        backend_refs.join(", ")
    )
}

fn cargo_bin_path() -> PathBuf {
    // Probing target/{release,debug} ran a stale release build under a debug
    // `cargo test` and ignored CARGO_TARGET_DIR; cargo builds this exact binary.
    PathBuf::from(env!("CARGO_BIN_EXE_portail"))
}
