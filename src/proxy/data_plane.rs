//! Tokio-based data plane — one TcpListener (or UdpSocket) per (address, port),
//! one accept loop per listener. Accept work is distributed across the Tokio
//! runtime's worker threads, not via per-core SO_REUSEPORT fan-out (which adds
//! task count without throughput on a single shared runtime).
//!
//! On shutdown, the data plane stops accepting new connections and waits up to
//! `DRAIN_TIMEOUT` for in-flight connections to finish.

use anyhow::Result;
use arc_swap::{ArcSwap, ArcSwapOption};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::net::{TcpListener, UdpSocket};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::config::{CertificateRef, ListenerConfig, PerformanceConfig, Protocol};
use crate::logging::{info, warn};
use crate::proxy::health::HealthRegistry;
use crate::proxy::tls::{self, TlsKind, TlsPlan};
use crate::proxy::udp_worker;
use crate::proxy::worker;
use crate::routing::{BackendSelector, RouteTable};

/// What a TCP port serves, read by its accept loops per connection:
/// `None` for plain bytes (HTTP, TCP), a plan for TLS (HTTPS, TLS
/// passthrough). Every endpoint bound for the port shares it.
pub(crate) type PortPlan = Arc<ArcSwapOption<TlsPlan>>;

/// A terminating config and the fingerprint of the certs it was built
/// from: a pass with the same certs reuses it.
type BuiltConfig = (u64, Arc<rustls::ServerConfig>);

struct TcpListenerEntry {
    port: u16,
    /// Socket actually bound (the Service targetPort when decoupled from `port`).
    bound_port: u16,
    address: Option<String>,
    listener: TcpListener,
    plan: PortPlan,
}

struct UdpListenerEntry {
    port: u16,
    bound_port: u16,
    address: Option<String>,
    socket: UdpSocket,
}

/// State of one bound (address, port) endpoint.
enum ListenerSlot {
    /// Socket bound at construction, accept loop not yet spawned — the
    /// standalone-mode window between `new()` and `start()`. Counts as
    /// bound: the kernel is already queueing connections on the socket.
    Pending,
    /// The token is this listener's own (a child of the process shutdown):
    /// cancelling it tears down just this accept loop, so a listener removed
    /// from the Gateway spec stops serving without touching its siblings.
    Running(JoinHandle<()>, CancellationToken),
}

impl ListenerSlot {
    fn alive(&self) -> bool {
        match self {
            ListenerSlot::Pending => true,
            ListenerSlot::Running(handle, _) => !handle.is_finished(),
        }
    }
}

pub struct DataPlane {
    tcp_listeners: Vec<TcpListenerEntry>,
    udp_listeners: Vec<UdpListenerEntry>,
    /// One accept-loop slot per bound (address, bound-port) endpoint.
    /// Readiness reads LIVE task state from this map — a finished task no
    /// longer counts as bound, which keeps /readyz and Programmed truthful
    /// when an accept loop dies. (Its add-only predecessor had no way back:
    /// a dead listener stayed "bound" until pod restart, refusing every
    /// connection while readiness stayed green.)
    listener_tasks: std::collections::HashMap<(Option<String>, u16), ListenerSlot>,
    /// Pod readiness flag shared with the admin endpoint (`/readyz`), flipped
    /// false the moment an accept loop exits outside shutdown — no waiting
    /// for the next reconcile pass to notice. `None` in standalone mode.
    ready_flag: Option<Arc<AtomicBool>>,
    worker_config: worker::WorkerConfig,
    health: Arc<HealthRegistry>,
    /// Cloned into every UDP worker, the controller's too: a rebuilt one
    /// would split the default session budget.
    udp_sessions: udp_worker::SessionLimits,
    shutdown: CancellationToken,
    active_connections: Arc<AtomicUsize>,
    /// Per published TCP port, every pass's plan (see [`PortPlan`]).
    plans: HashMap<u16, PortPlan>,
    /// Per port with HTTPS listeners, the config terminating for them.
    /// Dropped with the last of them: no cert outlives its listener.
    terminate_configs: HashMap<u16, BuiltConfig>,
    /// Shared backend selector across all listeners for correct weighted distribution.
    selector: Arc<BackendSelector>,
    /// Process-wide backend pool when `performance.backendPoolScope: process`;
    /// `None` keeps the default per-connection pools.
    shared_pool: Option<Arc<crate::proxy::backend_pool::SharedBackendPool>>,
}

/// Maximum time to wait for in-flight connections on shutdown.
const DRAIN_TIMEOUT: Duration = Duration::from_secs(30);

/// Stops a [`DataPlane`] and waits out its connections, without access to
/// the data plane itself (see [`DataPlane::drain_handle`]).
pub struct Drain {
    shutdown: CancellationToken,
    active_connections: Arc<AtomicUsize>,
}

impl Drain {
    /// Stop every accept loop, then wait up to `DRAIN_TIMEOUT` for the
    /// in-flight connections to finish.
    pub async fn run(self) {
        info!("Data plane shutting down, draining in-flight connections");
        self.shutdown.cancel();

        let active = self.active_connections;
        let drain_result = tokio::time::timeout(DRAIN_TIMEOUT, async {
            loop {
                let count = active.load(Ordering::Acquire);
                if count == 0 {
                    break;
                }
                info!("Waiting for {} in-flight connection(s) to drain", count);
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        })
        .await;

        match drain_result {
            Ok(()) => info!("All connections drained"),
            Err(_) => {
                let remaining = active.load(Ordering::Acquire);
                warn!(
                    "Drain timeout after {:?}: {} connections still active",
                    DRAIN_TIMEOUT, remaining
                );
            }
        }
        info!("Data plane shutdown complete");
    }
}

/// Idle backend connections retained per backend identity, in whichever pool
/// scope is configured. Not user-tunable today.
const DEFAULT_MAX_IDLE_PER_BACKEND: usize = 64;

/// Merge TLS cert refs across all HTTPS listeners on the same port.
///
/// One TCP listener per port serves traffic for every (port, hostname) scope,
/// so its `ServerConfig` must hold every hostname's cert. Building per-listener
/// would let the last write clobber earlier listeners and strand SNIs — e.g.
/// `example.org` via a catch-all listener gets dropped when a sibling
/// `https-with-hostname` listener is processed last, hanging its TLS handshake
/// until the client times out. An HTTPS listener without certs still names
/// its port: the build then fails for it, and says so.
pub(crate) fn merge_cert_refs_by_port(
    listener_configs: &[ListenerConfig],
) -> HashMap<u16, Vec<CertificateRef>> {
    let mut merged: HashMap<u16, Vec<CertificateRef>> = HashMap::new();
    // A listener bound on several addresses comes once per address. The
    // hostname is part of the key: one Secret on two listeners serves two
    // SNIs.
    let mut seen = std::collections::HashSet::new();
    for listener_cfg in listener_configs {
        if listener_cfg.protocol != Protocol::HTTPS {
            continue;
        }
        let refs = merged.entry(listener_cfg.port).or_default();
        let Some(tls_cfg) = &listener_cfg.tls else {
            continue;
        };
        for r in &tls_cfg.certificate_refs {
            let key = (
                listener_cfg.port,
                r.namespace.as_deref(),
                r.name.as_str(),
                r.hostname.as_deref(),
            );
            if seen.insert(key) {
                refs.push(r.clone());
            }
        }
    }
    merged
}

/// The plan of every TCP port `listener_configs` name, from those listeners
/// alone (see [`PortPlan`]): a TLS plan when an HTTPS or TLS passthrough
/// listener is on it, which TLS then is for every connection, else plain.
/// The terminating config is rebuilt only when its port's merged certs
/// changed (`built`, kept for ports with HTTPS listeners only). Failures
/// come back per port so construction can hard-fail while the reconcile
/// path retries next pass; such a port terminates nothing meanwhile.
fn plan_ports(
    listener_configs: &[ListenerConfig],
    cert_dir: &std::path::Path,
    http2: bool,
    built: &mut HashMap<u16, BuiltConfig>,
) -> (HashMap<u16, Option<TlsPlan>>, Vec<(u16, String)>) {
    let mut plans: HashMap<u16, Option<TlsPlan>> = HashMap::new();
    for l in listener_configs {
        let kind = match l.protocol {
            Protocol::UDP => continue,
            Protocol::HTTP | Protocol::TCP => None,
            Protocol::HTTPS => Some(TlsKind::Terminate),
            Protocol::TLS if l.is_tls_passthrough() => Some(TlsKind::Passthrough),
            // TLS Terminate is not served: kept out by validation and the
            // controller alike.
            Protocol::TLS => continue,
        };
        let plan = plans.entry(l.port).or_default();
        if let Some(kind) = kind {
            plan.get_or_insert_with(TlsPlan::default)
                .add(l.hostname.as_deref(), kind);
        }
    }

    let merged = merge_cert_refs_by_port(listener_configs);
    built.retain(|port, _| merged.contains_key(port));
    let mut errors = Vec::new();
    for (port, refs) in merged {
        let fingerprint = compute_cert_fingerprint(&refs);
        let config = match built.get(&port) {
            Some((held, config)) if *held == fingerprint => Some(config.clone()),
            _ => match tls::build_server_config(&refs, cert_dir, http2) {
                Ok(config) => {
                    let config = Arc::new(config);
                    built.insert(port, (fingerprint, config.clone()));
                    Some(config)
                }
                Err(e) => {
                    built.remove(&port);
                    errors.push((port, format!("TLS config build failed: {}", e)));
                    None
                }
            },
        };
        if let Some(Some(plan)) = plans.get_mut(&port) {
            plan.terminate = config;
        }
    }
    (plans, errors)
}

/// Whether a socket on a port with this plan could serve a connection: a
/// TLS port whose HTTPS certs failed and that forwards nothing is not
/// bound.
fn serves(plan: Option<&TlsPlan>) -> bool {
    plan.is_none_or(TlsPlan::serves)
}

/// Compute a fingerprint of all cert PEM bytes for change detection.
/// Uses FNV-1a for speed — collision resistance isn't critical here,
/// a false positive just triggers one extra ServerConfig rebuild.
fn compute_cert_fingerprint(refs: &[CertificateRef]) -> u64 {
    use std::hash::Hasher;
    let mut hasher = fnv::FnvHasher::default();
    for r in refs {
        if let Some(pem) = &r.cert_pem {
            hasher.write(pem);
        }
        if let Some(pem) = &r.key_pem {
            hasher.write(pem);
        }
        hasher.write(r.name.as_bytes());
        if let Some(h) = &r.hostname {
            hasher.write(h.as_bytes());
        }
    }
    hasher.finish()
}

impl DataPlane {
    /// Create one TcpListener / UdpSocket per (address, port) listener config.
    /// The Tokio runtime multiplexes accepts across its worker threads; no
    /// per-core SO_REUSEPORT fan-out is needed for a single-process data plane.
    pub fn new(
        listeners: &[ListenerConfig],
        performance_config: &PerformanceConfig,
        cert_dir: &std::path::Path,
    ) -> Result<Self> {
        let health = Arc::new(HealthRegistry::new());

        let mut tcp_listeners = Vec::new();
        let mut udp_listeners = Vec::new();

        // Per-port plans, their terminating configs from the MERGED cert
        // refs, the reconcile path's own, so siblings on a shared port
        // cannot clobber each other's SNI bindings here either. A bad cert
        // set fails construction outright: standalone mode has no retry pass.
        let mut terminate_configs = HashMap::new();
        let (plans, tls_errors) = plan_ports(
            listeners,
            cert_dir,
            performance_config.http2,
            &mut terminate_configs,
        );
        if let Some((port, err)) = tls_errors.into_iter().next() {
            return Err(anyhow::anyhow!("port {}: {}", port, err));
        }
        let plans: HashMap<u16, PortPlan> = plans
            .into_iter()
            .map(|(port, plan)| (port, Arc::new(ArcSwapOption::from_pointee(plan))))
            .collect();

        // One socket per endpoint: siblings on a shared port ride the same
        // socket (routing is port-keyed), and a second bind would fail
        // EADDRINUSE against our own listener.
        let mut bound_endpoints: std::collections::HashSet<(Option<String>, u16)> =
            std::collections::HashSet::new();

        for listener_cfg in listeners.iter() {
            // `port` is the published identity (routing/TLS key); `bound_port` is
            // the socket we actually bind — the Service's targetPort when set.
            let port = listener_cfg.port;
            let bound_port = listener_cfg.target_port.unwrap_or(port);
            let plan = match listener_cfg.protocol {
                Protocol::UDP => None,
                // A listener no plan took (TLS Terminate) binds nothing.
                _ => match plans.get(&port) {
                    Some(plan) => Some(plan),
                    None => continue,
                },
            };
            if !bound_endpoints.insert((listener_cfg.address.clone(), bound_port)) {
                continue;
            }
            match plan {
                None => {
                    let std_socket = create_udp_socket(
                        bound_port,
                        listener_cfg.address.as_deref(),
                        listener_cfg.interface.as_deref(),
                    )?;
                    let tokio_socket = UdpSocket::from_std(std_socket)?;

                    udp_listeners.push(UdpListenerEntry {
                        port,
                        bound_port,
                        address: listener_cfg.address.clone(),
                        socket: tokio_socket,
                    });
                }
                Some(plan) => {
                    let std_listener = create_tcp_listener(
                        bound_port,
                        listener_cfg.address.as_deref(),
                        listener_cfg.interface.as_deref(),
                    )?;
                    let tokio_listener = TcpListener::from_std(std_listener)?;

                    tcp_listeners.push(TcpListenerEntry {
                        port,
                        bound_port,
                        address: listener_cfg.address.clone(),
                        listener: tokio_listener,
                        plan: plan.clone(),
                    });
                }
            }
        }
        // Sockets exist from here on; `start()` attaches accept loops.
        let listener_tasks = bound_endpoints
            .into_iter()
            .map(|endpoint| (endpoint, ListenerSlot::Pending))
            .collect();

        Ok(Self {
            tcp_listeners,
            udp_listeners,
            listener_tasks,
            ready_flag: None,
            worker_config: worker::WorkerConfig::from_perf(
                performance_config,
                DEFAULT_MAX_IDLE_PER_BACKEND,
            ),
            health,
            udp_sessions: udp_worker::SessionLimits::from_perf(performance_config),
            shutdown: CancellationToken::new(),
            active_connections: Arc::new(AtomicUsize::new(0)),
            plans,
            terminate_configs,
            selector: Arc::new(BackendSelector::new()),
            shared_pool: match performance_config.backend_pool_scope {
                crate::config::PoolScope::Process => Some(Arc::new(
                    crate::proxy::backend_pool::SharedBackendPool::new(
                        DEFAULT_MAX_IDLE_PER_BACKEND,
                        performance_config.backend_timeout,
                    ),
                )),
                crate::config::PoolScope::Connection => None,
            },
        })
    }

    /// Share the pod readiness flag so a dying accept loop can flip it
    /// immediately (the admin endpoint serves it as `/readyz`). Wired by the
    /// Kubernetes controller at startup; standalone mode has no probe to feed.
    pub fn set_ready_flag(&mut self, flag: Arc<AtomicBool>) {
        self.ready_flag = Some(flag);
    }

    fn endpoint_alive(&self, endpoint: &(Option<String>, u16)) -> bool {
        self.listener_tasks
            .get(endpoint)
            .is_some_and(ListenerSlot::alive)
    }

    /// Spawn an accept loop and record it under its endpoint. The loop gets
    /// its own child token of the process shutdown, so it can be torn down
    /// individually (spec removed the listener) or collectively.
    ///
    /// The wrapper is the death-watch: an accept loop returns exactly once,
    /// on cancellation, so any other exit means its socket is gone — flip
    /// readiness right away instead of letting `/readyz` vouch for a port
    /// nothing is bound to. The finished handle stays in the map until the
    /// next ensure pass reaps and rebinds it. (A panic bypasses the wrapper
    /// body: under `panic = "abort"` the whole process dies with it — its
    /// own honest signal — and `JoinHandle::is_finished` covers the unwind
    /// case.)
    fn spawn_listener_task<F, Fut>(&mut self, endpoint: (Option<String>, u16), make_task: F)
    where
        F: FnOnce(CancellationToken) -> Fut,
        Fut: std::future::Future<Output = ()> + Send + 'static,
    {
        let cancel = self.shutdown.child_token();
        let ready_flag = self.ready_flag.clone();
        let desc = format!("{}:{}", endpoint.0.as_deref().unwrap_or("*"), endpoint.1);
        let task = make_task(cancel.clone());
        let watch_cancel = cancel.clone();
        let handle = tokio::spawn(async move {
            task.await;
            if !watch_cancel.is_cancelled() {
                warn!("Accept loop for {} exited unexpectedly", desc);
                if let Some(flag) = ready_flag {
                    flag.store(false, Ordering::Release);
                }
            }
        });
        self.listener_tasks
            .insert(endpoint, ListenerSlot::Running(handle, cancel));
    }

    /// The (address, bound port) endpoints `listener_configs` bind: each
    /// listener on its own address, or every interface. A wildcard endpoint
    /// stands for the specific ones on its port: it already takes their
    /// traffic, routing is port-keyed, and Linux refuses a specific bind
    /// beside a listening wildcard.
    fn desired_endpoints(
        listener_configs: &[ListenerConfig],
    ) -> std::collections::HashSet<(Option<String>, u16)> {
        let mut desired: std::collections::HashSet<(Option<String>, u16)> = listener_configs
            .iter()
            .map(|lc| (lc.address.clone(), lc.target_port.unwrap_or(lc.port)))
            .collect();
        let wildcard_ports: std::collections::HashSet<u16> = desired
            .iter()
            .filter(|(addr, _)| addr.is_none())
            .map(|(_, port)| *port)
            .collect();
        desired.retain(|(addr, port)| addr.is_none() || !wildcard_ports.contains(port));
        desired
    }

    /// Reap dead accept loops, and close the endpoints `listener_configs`
    /// no longer want: a listener removed from the spec, or a wildcard
    /// whose port moved to specific addresses (or the reverse). Returns the
    /// closed loops. Each releases its socket as it exits, and until then a
    /// bind overlapping it fails: await them, then
    /// [`Self::add_tcp_listeners`] the same listeners.
    pub fn close_unwanted(&mut self, listener_configs: &[ListenerConfig]) -> Vec<JoinHandle<()>> {
        // A finished task means its socket is gone (the listener lives
        // inside the task), so the endpoint must stop counting as bound;
        // the bind loop then re-opens it this same pass.
        self.listener_tasks.retain(|(addr, bound_port), slot| {
            if slot.alive() {
                true
            } else {
                warn!(
                    "Listener {}:{} accept loop is dead; rebinding this pass",
                    addr.as_deref().unwrap_or("*"),
                    bound_port
                );
                false
            }
        });

        let desired = Self::desired_endpoints(listener_configs);
        self.listener_tasks
            // Pre-start sockets (standalone mode) are never managed here.
            .extract_if(|endpoint, slot| {
                !desired.contains(endpoint) && !matches!(slot, ListenerSlot::Pending)
            })
            .filter_map(|(endpoint, slot)| {
                info!(
                    "Listener {}:{} no longer in spec; closing",
                    endpoint.0.as_deref().unwrap_or("*"),
                    endpoint.1
                );
                match slot {
                    ListenerSlot::Running(handle, cancel) => {
                        cancel.cancel();
                        Some(handle)
                    }
                    ListenerSlot::Pending => None,
                }
            })
            .collect()
    }

    /// Bind the endpoints `listener_configs` want that have no live accept
    /// loop: each listener on its own `address` (one listener per bind
    /// address, expanded by the caller), or every interface. One listener
    /// per (address, port); accept work is distributed by the Tokio runtime.
    /// Publishes every TCP port's plan first ([`plan_ports`]): a socket
    /// already bound serves its next connection by it, so a port changes
    /// protocol, gains or sheds a listener or a cert, without a rebind.
    /// Returns `(ports_opened, errors)`.
    ///
    /// The second half of a pass: [`Self::close_unwanted`] of the same
    /// listeners first, its loops awaited, or a bind overlapping a socket
    /// still open fails.
    pub fn add_tcp_listeners(
        &mut self,
        listener_configs: &[ListenerConfig],
        routes: Arc<ArcSwap<RouteTable>>,
        performance_config: &crate::config::PerformanceConfig,
    ) -> (usize, Vec<(u16, String)>) {
        let mut opened = 0usize;
        let desired = Self::desired_endpoints(listener_configs);

        // build_server_config (full PEM parse + key load for every ref) runs
        // only when a port's merged cert bytes changed, and once per port.
        // The bind loop below always runs, so a newly appeared bind address
        // still gets bound when the certs are unchanged.
        let (plans, mut errors) = plan_ports(
            listener_configs,
            std::path::Path::new("/unused"),
            performance_config.http2,
            &mut self.terminate_configs,
        );
        self.plans.retain(|port, _| plans.contains_key(port));
        for (port, plan) in plans {
            match self.plans.get(&port) {
                Some(slot) => slot.store(plan.map(Arc::new)),
                None => {
                    self.plans
                        .insert(port, Arc::new(ArcSwapOption::from_pointee(plan)));
                }
            }
        }

        for listener_cfg in listener_configs {
            // `port` is the published identity (routing/TLS key); `bound_port` is
            // the socket we actually bind — the Service's targetPort when set.
            let port = listener_cfg.port;
            let bound_port = listener_cfg.target_port.unwrap_or(port);

            let plan = match listener_cfg.protocol {
                Protocol::UDP => None,
                // A listener no plan took (TLS Terminate) binds nothing; nor
                // does a port that could serve no connection (its certs
                // failed, error already recorded).
                _ => match self.plans.get(&port) {
                    Some(plan) if serves(plan.load().as_deref()) => Some(plan.clone()),
                    _ => continue,
                },
            };

            let address = listener_cfg.address.as_deref();
            let endpoint = (listener_cfg.address.clone(), bound_port);
            // Already bound, or a specific address the port's wildcard
            // stands for.
            if self.endpoint_alive(&endpoint) || !desired.contains(&endpoint) {
                continue;
            }

            if let Some(plan) = plan {
                match self.bind_tcp(listener_cfg, endpoint, plan, &routes, performance_config) {
                    Ok(()) => opened += 1,
                    Err(e) => errors.push((port, e)),
                }
            } else {
                match create_udp_socket(bound_port, address, listener_cfg.interface.as_deref()) {
                    Ok(std_socket) => match UdpSocket::from_std(std_socket) {
                        Ok(tokio_socket) => {
                            let routes = routes.clone();
                            let health = self.health.clone();
                            let limits = self.udp_sessions.clone();
                            let socket = Arc::new(tokio_socket);

                            self.spawn_listener_task(endpoint, |cancel| async move {
                                udp_worker::run_udp_worker(
                                    socket, port, routes, limits, cancel, health,
                                )
                                .await;
                            });
                            opened += 1;
                        }
                        Err(e) => errors.push((port, format!("UDP from_std: {}", e))),
                    },
                    Err(e) => errors.push((port, format!("UDP bind: {}", e))),
                }
            }
        }
        (opened, errors)
    }

    /// Bind `listener_cfg`'s `endpoint` and spawn its accept loop, which
    /// reads `plan` per connection.
    fn bind_tcp(
        &mut self,
        listener_cfg: &ListenerConfig,
        endpoint: (Option<String>, u16),
        plan: PortPlan,
        routes: &Arc<ArcSwap<RouteTable>>,
        performance_config: &crate::config::PerformanceConfig,
    ) -> Result<(), String> {
        let std_listener = create_tcp_listener(
            endpoint.1,
            endpoint.0.as_deref(),
            listener_cfg.interface.as_deref(),
        )
        .map_err(|e| format!("TCP bind: {}", e))?;
        let tokio_listener =
            TcpListener::from_std(std_listener).map_err(|e| format!("TCP from_std: {}", e))?;
        // The controller's perf config takes precedence over the startup
        // one for dynamic listeners.
        let cfg = worker::WorkerConfig::from_perf(
            performance_config,
            self.worker_config.max_idle_per_backend,
        );
        let ctx = worker::ListenerCtx {
            server_port: listener_cfg.port,
            routes: routes.clone(),
            cfg,
            plan,
            health: self.health.clone(),
            selector: self.selector.clone(),
            active_connections: self.active_connections.clone(),
            shared_pool: self.shared_pool.clone(),
        };
        self.spawn_listener_task(endpoint, |cancel| async move {
            worker::run_worker(tokio_listener, ctx, cancel).await;
        });
        Ok(())
    }

    /// Start async listener tasks (one accept loop per TCP listener / UDP socket).
    /// Upgrades each construction-time `Pending` slot to `Running`.
    pub fn start(&mut self, routes: Arc<ArcSwap<RouteTable>>) {
        let tcp_count = self.tcp_listeners.len();
        let udp_count = self.udp_listeners.len();

        let tcp_entries = std::mem::take(&mut self.tcp_listeners);
        for entry in tcp_entries {
            let TcpListenerEntry {
                port,
                bound_port,
                address,
                listener,
                plan,
            } = entry;
            let ctx = worker::ListenerCtx {
                server_port: port,
                routes: routes.clone(),
                cfg: self.worker_config,
                plan,
                health: self.health.clone(),
                selector: self.selector.clone(),
                active_connections: self.active_connections.clone(),
                shared_pool: self.shared_pool.clone(),
            };
            self.spawn_listener_task((address, bound_port), |cancel| async move {
                worker::run_worker(listener, ctx, cancel).await;
            });
        }

        let udp_entries = std::mem::take(&mut self.udp_listeners);
        for entry in udp_entries {
            let socket = Arc::new(entry.socket);
            let routes = routes.clone();
            let health = self.health.clone();
            let limits = self.udp_sessions.clone();
            let port = entry.port;

            self.spawn_listener_task((entry.address, entry.bound_port), |cancel| async move {
                udp_worker::run_udp_worker(socket, port, routes, limits, cancel, health).await;
            });
        }

        info!(
            "Data plane started: {} TCP + {} UDP listener tasks",
            tcp_count, udp_count
        );
    }

    /// The handle that drains this data plane. Take it before sharing the
    /// data plane: the controller holds a clone for as long as it runs, so
    /// a drain that had to own the data plane would never run.
    pub fn drain_handle(&self) -> Drain {
        Drain {
            shutdown: self.shutdown.clone(),
            active_connections: self.active_connections.clone(),
        }
    }

    /// Check if all the specified (address, port) endpoints are bound with a
    /// LIVE accept loop (or a construction-time socket awaiting `start()`).
    /// A dead accept loop does NOT count: readiness and Programmed must go
    /// false the moment a listener can no longer accept, so the reconcile
    /// safety net rebinds instead of trusting stale bookkeeping.
    pub fn is_ready_for_endpoints(&self, required: &[(Option<String>, u16)]) -> bool {
        required.iter().all(|ep| {
            self.endpoint_alive(ep)
                // A wildcard socket on the port already accepts traffic for
                // any specific address; routing is keyed on the port. It is
                // still open only while the spec wants it (close_unwanted).
                // The reverse does NOT hold: a required wildcard bind is not
                // satisfied by an address-specific one.
                || (ep.0.is_some() && self.endpoint_alive(&(None, ep.1)))
        })
    }

    /// Get a reference to the shared health registry.
    pub fn health(&self) -> &Arc<HealthRegistry> {
        &self.health
    }
}

fn create_tcp_listener(
    port: u16,
    bind_addr: Option<&str>,
    interface: Option<&str>,
) -> Result<std::net::TcpListener> {
    use socket2::{Domain, Protocol, Socket, Type};

    let ip: std::net::IpAddr = bind_addr.unwrap_or("0.0.0.0").parse().map_err(|e| {
        anyhow::anyhow!("Invalid bind address '{}': {}", bind_addr.unwrap_or(""), e)
    })?;
    let domain = if ip.is_ipv6() {
        Domain::IPV6
    } else {
        Domain::IPV4
    };
    let socket = Socket::new(domain, Type::STREAM, Some(Protocol::TCP))?;
    socket.set_nonblocking(true)?;
    // SO_REUSEADDR lets a restart re-bind a port still held in TIME-WAIT by an
    // older connection. Standard for any long-running proxy / server; unrelated
    // to SO_REUSEPORT (per-core fan-out, removed in c010a19). Without this, a
    // graceful restart waits up to net.ipv4.tcp_fin_timeout (60s by default)
    // per port before it can accept again.
    socket.set_reuse_address(true)?;

    if let Some(iface) = interface {
        socket.bind_device(Some(iface.as_bytes()))?;
    }

    let addr = std::net::SocketAddr::new(ip, port);
    socket.bind(&addr.into())?;
    socket.listen(1024)?;

    Ok(socket.into())
}

fn create_udp_socket(
    port: u16,
    bind_addr: Option<&str>,
    interface: Option<&str>,
) -> Result<std::net::UdpSocket> {
    use socket2::{Domain, Protocol, Socket, Type};

    let ip: std::net::IpAddr = bind_addr.unwrap_or("0.0.0.0").parse().map_err(|e| {
        anyhow::anyhow!("Invalid bind address '{}': {}", bind_addr.unwrap_or(""), e)
    })?;
    let domain = if ip.is_ipv6() {
        Domain::IPV6
    } else {
        Domain::IPV4
    };
    let socket = Socket::new(domain, Type::DGRAM, Some(Protocol::UDP))?;
    socket.set_nonblocking(true)?;

    if let Some(iface) = interface {
        socket.bind_device(Some(iface.as_bytes()))?;
    }

    let addr = std::net::SocketAddr::new(ip, port);
    socket.bind(&addr.into())?;

    Ok(socket.into())
}

#[cfg(test)]
pub(crate) mod test_ports {
    use std::hash::BuildHasher;

    /// A port free on every interface, below the ephemeral range. The
    /// kernel hands ephemeral ports to this process's client sockets, and
    /// one took a wildcard port between its probe and its bind.
    pub(crate) fn wildcard() -> u16 {
        let range = std::fs::read_to_string("/proc/sys/net/ipv4/ip_local_port_range").unwrap();
        let first_ephemeral: u16 = range.split_whitespace().next().unwrap().parse().unwrap();
        let unprivileged = 1024..first_ephemeral;
        let len = unprivileged.len();
        // Tests running beside this one start elsewhere.
        let start = std::hash::RandomState::new().hash_one(()) as usize;
        (0..len)
            .map(|i| unprivileged.start + ((start + i) % len) as u16)
            .find(|&port| std::net::TcpListener::bind(("0.0.0.0", port)).is_ok())
            .expect("a free port below the ephemeral range")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{CertificateRef, ListenerConfig, Protocol as P, TlsConfig, TlsMode as TM};

    fn https_listener(
        name: &str,
        port: u16,
        hostname: Option<&str>,
        cert_name: &str,
    ) -> ListenerConfig {
        ListenerConfig {
            name: name.into(),
            protocol: P::HTTPS,
            port,
            target_port: None,
            hostname: hostname.map(str::to_string),
            address: None,
            interface: None,
            tls: Some(TlsConfig {
                mode: TM::Terminate,
                certificate_refs: vec![CertificateRef {
                    name: cert_name.into(),
                    hostname: hostname.map(str::to_string),
                    cert_pem: Some(b"PEM-CERT".to_vec()),
                    key_pem: Some(b"PEM-KEY".to_vec()),
                    namespace: None,
                }],
            }),
            rate_limit: None,
        }
    }

    /// Two TLS-Terminate listeners on the same port — one catch-all, one
    /// hostname-scoped — must produce one merged cert-ref list under that
    /// port, in listener order. Regression for the bug where per-listener
    /// `existing.update(config)` clobbered sibling SNI bindings and hung
    /// the catch-all's TLS (Gateway API conformance HTTPRouteHTTPSListener).
    #[test]
    fn merges_same_port_listeners_into_one_cert_list() {
        let listeners = vec![
            https_listener("https", 443, None, "default-cert"),
            https_listener(
                "https-with-hostname",
                443,
                Some("second.example.org"),
                "default-cert",
            ),
        ];
        let merged = merge_cert_refs_by_port(&listeners);

        assert_eq!(merged.len(), 1, "one port expected");
        let refs = &merged[&443];
        assert_eq!(refs.len(), 2, "both listeners' refs must appear");
        assert_eq!(
            refs[0].hostname, None,
            "catch-all listener's ref comes first"
        );
        assert_eq!(
            refs[1].hostname.as_deref(),
            Some("second.example.org"),
            "hostname-scoped listener's ref preserves its hostname"
        );
    }

    /// Listeners on different ports stay isolated; non-TLS / Passthrough
    /// listeners contribute nothing.
    #[test]
    fn merge_respects_port_and_mode() {
        let mut passthrough = https_listener("tlsp", 9443, Some("passthrough.example.org"), "p");
        passthrough.protocol = P::TLS;
        passthrough.tls.as_mut().unwrap().mode = TM::Passthrough;

        let listeners = vec![
            https_listener("a", 443, None, "ca"),
            https_listener("b", 8443, Some("b.example.org"), "cb"),
            passthrough,
            ListenerConfig {
                name: "http-only".into(),
                protocol: P::HTTP,
                port: 80,
                target_port: None,
                hostname: None,
                address: None,
                interface: None,
                tls: None,
                rate_limit: None,
            },
        ];
        let merged = merge_cert_refs_by_port(&listeners);

        assert_eq!(merged.keys().copied().collect::<Vec<_>>().len(), 2);
        assert!(merged.contains_key(&443));
        assert!(merged.contains_key(&8443));
        assert!(
            !merged.contains_key(&9443),
            "Passthrough must not contribute"
        );
        assert!(!merged.contains_key(&80), "HTTP must not contribute");
    }

    /// Refs merge by their Secret's key and the hostname they serve, never
    /// by comparing PEM bytes pairwise: one Secret seen twice merges once,
    /// whatever bytes each copy carries.
    #[test]
    fn merged_refs_dedup_by_secret_key() {
        let mut a = https_listener("a", 443, Some("a.example.org"), "cert");
        a.tls.as_mut().unwrap().certificate_refs[0].namespace = Some("ns1".into());
        let mut again = a.clone();
        again.address = Some("10.0.0.2".into());
        again.tls.as_mut().unwrap().certificate_refs[0].cert_pem = Some(b"ROTATED".to_vec());
        let mut other_ns = a.clone();
        other_ns.tls.as_mut().unwrap().certificate_refs[0].namespace = Some("ns2".into());
        let merged = merge_cert_refs_by_port(&[a, again, other_ns]);
        let namespaces: Vec<_> = merged[&443]
            .iter()
            .map(|r| r.namespace.as_deref())
            .collect();
        assert_eq!(namespaces, vec![Some("ns1"), Some("ns2")]);
    }

    /// A listener bound on two addresses comes as two copies. Each added
    /// its refs again: the fingerprint moved with the address count, and
    /// a new address rebuilt the port's ServerConfig with unchanged certs.
    #[test]
    fn a_listener_on_several_addresses_merges_its_refs_once() {
        let listener = https_listener("https", 443, Some("a.example.org"), "c");
        let on = |address: &str| ListenerConfig {
            address: Some(address.to_string()),
            ..listener.clone()
        };
        let one = merge_cert_refs_by_port(&[on("10.0.0.1")]);
        let two = merge_cert_refs_by_port(&[on("10.0.0.1"), on("10.0.0.2")]);
        assert_eq!(two[&443].len(), 1);
        assert_eq!(
            compute_cert_fingerprint(&one[&443]),
            compute_cert_fingerprint(&two[&443])
        );
    }

    /// Fingerprint over a port's merged refs must be stable across calls —
    /// if it weren't, every reconcile would flip the cache between siblings
    /// and re-trigger hot-reloads forever (the symptom that originally
    /// surfaced the bug in `Hot-reloaded TLS config for port 443` log spam).
    #[test]
    fn merged_fingerprint_is_stable_across_calls() {
        let listeners = vec![
            https_listener("https", 443, None, "c"),
            https_listener("https-with-hostname", 443, Some("h.example.org"), "c"),
        ];
        let m1 = merge_cert_refs_by_port(&listeners);
        let m2 = merge_cert_refs_by_port(&listeners);
        assert_eq!(
            compute_cert_fingerprint(&m1[&443]),
            compute_cert_fingerprint(&m2[&443]),
            "deterministic input must hash deterministically",
        );
    }

    /// A listener with a `target_port` binds that port (the Service's
    /// targetPort), not the published `port`, and readiness keys on it.
    #[tokio::test]
    async fn binds_target_port_not_published() {
        let listeners = vec![ListenerConfig {
            name: "http".into(),
            protocol: P::HTTP,
            port: 21080,
            target_port: Some(31080),
            hostname: None,
            address: None,
            interface: None,
            tls: None,
            rate_limit: None,
        }];
        let perf = test_perf();
        let dp = DataPlane::new(&listeners, &perf, std::path::Path::new("/unused")).unwrap();

        // Readiness keys on the bound (target) port, not the published port.
        assert!(dp.is_ready_for_endpoints(&[(None, 31080)]));
        assert!(!dp.is_ready_for_endpoints(&[(None, 21080)]));

        // The socket really landed on the target port (held by the data plane)...
        assert!(
            std::net::TcpListener::bind(("0.0.0.0", 31080u16)).is_err(),
            "target port must be bound by the data plane"
        );
        // ...and the published port was never bound.
        assert!(
            std::net::TcpListener::bind(("0.0.0.0", 21080u16)).is_ok(),
            "published port must not be bound"
        );

        drop(dp);
    }

    fn test_perf() -> crate::config::PerformanceConfig {
        crate::config::PerformanceConfig {
            backend_timeout: std::time::Duration::from_secs(1),
            udp_session_timeout: std::time::Duration::from_secs(1),
            dns_refresh_interval: std::time::Duration::from_secs(1),
            client_header_timeout: std::time::Duration::from_secs(1),
            ..Default::default()
        }
    }

    /// Two Terminate listeners on one port taken through CONSTRUCTION
    /// (standalone mode) must share one merged acceptor: a handshake under
    /// either hostname's SNI succeeds. Before the fix each listener built
    /// its own acceptor from only its own refs and the second bind failed
    /// EADDRINUSE — the cert-clobber fix only covered the reconcile path.
    #[tokio::test]
    async fn construction_path_merges_certs_per_port() {
        let certs = ["a.example.org", "b.example.org"]
            .map(|cn| crate::proxy::tls::test_util::generate_test_cert(cn).map(|c| (cn, c)));
        let [Some((host_a, (cert_a, key_a))), Some((host_b, (cert_b, key_b)))] = certs else {
            return; // openssl unavailable
        };
        let mut la = https_listener("a", 23450, Some(host_a), "cert-a");
        la.tls.as_mut().unwrap().certificate_refs[0].cert_pem = Some(cert_a);
        la.tls.as_mut().unwrap().certificate_refs[0].key_pem = Some(key_a);
        la.address = Some("127.0.0.1".into());
        let mut lb = https_listener("b", 23450, Some(host_b), "cert-b");
        lb.tls.as_mut().unwrap().certificate_refs[0].cert_pem = Some(cert_b);
        lb.tls.as_mut().unwrap().certificate_refs[0].key_pem = Some(key_b);
        lb.address = Some("127.0.0.1".into());

        // In-memory PEMs only, so cert_dir is never consulted.
        let perf = test_perf();
        let mut dp = DataPlane::new(&[la, lb], &perf, std::path::Path::new("/unused")).unwrap();
        let routes = Arc::new(ArcSwap::from_pointee(RouteTable::new()));
        dp.start(routes);

        let client_cfg = Arc::new(
            rustls::ClientConfig::builder()
                .dangerous()
                .with_custom_certificate_verifier(Arc::new(crate::proxy::backend_pool::NoVerifier))
                .with_no_client_auth(),
        );
        for host in [host_a, host_b] {
            let tcp = tokio::net::TcpStream::connect(("127.0.0.1", 23450u16))
                .await
                .unwrap();
            let sni = rustls::pki_types::ServerName::try_from(host.to_string()).unwrap();
            let connector = tokio_rustls::TlsConnector::from(client_cfg.clone());
            let hs = connector.connect(sni, tcp).await;
            assert!(
                hs.is_ok(),
                "handshake under SNI {} failed: {:?} (cert not merged?)",
                host,
                hs.err()
            );
        }
    }

    /// A passthrough listener and an HTTPS one on a port share its socket,
    /// whichever comes first: SNI picks forwarding or termination. Listed
    /// first, the passthrough listener bound a socket that dropped every
    /// SNI its HTTPS sibling serves.
    #[tokio::test]
    async fn https_and_passthrough_share_a_socket_in_either_order() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        let Some((cert, key)) = crate::proxy::tls::test_util::generate_test_cert("app.example.org")
        else {
            return; // openssl unavailable
        };
        let backend = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let client_cfg = Arc::new(
            rustls::ClientConfig::builder()
                .dangerous()
                .with_custom_certificate_verifier(Arc::new(crate::proxy::backend_pool::NoVerifier))
                .with_no_client_auth(),
        );
        let perf = test_perf();
        for passthrough_first in [true, false] {
            let port = free_port();
            let mut https = https_listener("web", port, Some("app.example.org"), "cert");
            https.tls.as_mut().unwrap().certificate_refs[0].cert_pem = Some(cert.clone());
            https.tls.as_mut().unwrap().certificate_refs[0].key_pem = Some(key.clone());
            let mut passthrough = https_listener("db", port, Some("secure.example.org"), "-");
            passthrough.protocol = P::TLS;
            passthrough.tls = Some(TlsConfig {
                mode: TM::Passthrough,
                certificate_refs: vec![],
            });
            let listeners = if passthrough_first {
                vec![passthrough, https]
            } else {
                vec![https, passthrough]
            };
            let mut table = RouteTable::new();
            let forward = crate::routing::Backend::new(
                "127.0.0.1".into(),
                backend.local_addr().unwrap().port(),
            )
            .unwrap();
            table.add_tls_route(port, "secure.example.org", vec![forward]);
            let routes = Arc::new(ArcSwap::from_pointee(table));
            let mut dp = DataPlane::new(&[], &perf, std::path::Path::new("/unused")).unwrap();
            bind_until(&mut dp, &listeners, &routes, &perf, 1).await;

            let tcp = tokio::net::TcpStream::connect(("127.0.0.1", port))
                .await
                .unwrap();
            let sni = rustls::pki_types::ServerName::try_from("app.example.org").unwrap();
            let connector = tokio_rustls::TlsConnector::from(client_cfg.clone());
            let hs = connector.connect(sni, tcp).await;
            assert!(
                hs.is_ok(),
                "{passthrough_first}: terminated: {:?}",
                hs.err()
            );

            let name = rustls::pki_types::ServerName::try_from("secure.example.org").unwrap();
            let mut hello = Vec::new();
            rustls::ClientConnection::new(client_cfg.clone(), name)
                .unwrap()
                .write_tls(&mut hello)
                .unwrap();
            let mut client = tokio::net::TcpStream::connect(("127.0.0.1", port))
                .await
                .unwrap();
            client.write_all(&hello).await.unwrap();
            let (mut forwarded, _) = tokio::time::timeout(Duration::from_secs(5), backend.accept())
                .await
                .expect("passthrough reached the backend")
                .unwrap();
            let mut got = vec![0u8; hello.len()];
            forwarded.read_exact(&mut got).await.unwrap();
            assert_eq!(got, hello, "{passthrough_first}: passed through as sent");
        }
    }

    /// An HTTPS listener for `hostname` on `port`, its cert in memory.
    fn https_with_cert(
        port: u16,
        hostname: Option<&str>,
        (cert, key): &(Vec<u8>, Vec<u8>),
    ) -> ListenerConfig {
        let mut https = https_listener("web", port, hostname, "cert");
        let cert_ref = &mut https.tls.as_mut().unwrap().certificate_refs[0];
        cert_ref.cert_pem = Some(cert.clone());
        cert_ref.key_pem = Some(key.clone());
        https
    }

    fn passthrough_listener(port: u16, hostname: Option<&str>) -> ListenerConfig {
        ListenerConfig {
            protocol: P::TLS,
            tls: Some(TlsConfig {
                mode: TM::Passthrough,
                certificate_refs: vec![],
            }),
            ..https_listener("db", port, hostname, "-")
        }
    }

    /// Trusts any server; sends `sni`, or no SNI at all.
    fn client_config(sni: Option<&str>) -> (Arc<rustls::ClientConfig>, ServerName<'static>) {
        let mut config = rustls::ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(crate::proxy::backend_pool::NoVerifier))
            .with_no_client_auth();
        config.enable_sni = sni.is_some();
        let name = ServerName::try_from(sni.unwrap_or("unsent.test").to_string()).unwrap();
        (Arc::new(config), name)
    }

    use rustls::pki_types::ServerName;

    /// The cert the proxy on `port` presents for `sni`, or why it presents none.
    async fn presented(port: u16, sni: Option<&str>) -> Result<Vec<u8>, String> {
        let (config, name) = client_config(sni);
        let tcp = tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .map_err(|e| e.to_string())?;
        let handshake = tokio_rustls::TlsConnector::from(config).connect(name, tcp);
        match tokio::time::timeout(Duration::from_secs(5), handshake).await {
            Ok(Ok(tls)) => Ok(tls.get_ref().1.peer_certificates().unwrap()[0].to_vec()),
            Ok(Err(e)) => Err(e.to_string()),
            Err(_) => Err("handshake timed out".into()),
        }
    }

    fn der(pem: &[u8]) -> Vec<u8> {
        use rustls::pki_types::pem::PemObject;
        rustls::pki_types::CertificateDer::from_pem_slice(pem)
            .unwrap()
            .to_vec()
    }

    /// Send a ClientHello for `sni` to `port`: `Some` with what `backend`
    /// received, as sent, or `None` once the proxy closed the connection
    /// without reaching the backend.
    async fn hello_reaches(
        port: u16,
        sni: Option<&str>,
        backend: &tokio::net::TcpListener,
    ) -> Option<bool> {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let (config, name) = client_config(sni);
        let mut hello = Vec::new();
        rustls::ClientConnection::new(config, name)
            .unwrap()
            .write_tls(&mut hello)
            .unwrap();
        let mut client = tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .unwrap();
        client.write_all(&hello).await.unwrap();
        let (wait, mut byte) = (Duration::from_secs(5), [0u8; 1]);
        tokio::select! {
            accepted = backend.accept() => {
                let (mut forwarded, _) = accepted.unwrap();
                let mut got = vec![0u8; hello.len()];
                forwarded.read_exact(&mut got).await.unwrap();
                Some(got == hello)
            }
            read = tokio::time::timeout(wait, client.read(&mut byte)) => {
                assert!(!matches!(read, Ok(Ok(n)) if n > 0), "the proxy answered");
                assert!(read.is_ok(), "neither closed nor forwarded");
                None
            }
        }
    }

    /// One pass binding `listeners` on a fresh data plane over `table`.
    async fn serve(listeners: &[ListenerConfig], table: RouteTable) -> DataPlane {
        let perf = test_perf();
        let mut dp = DataPlane::new(&[], &perf, std::path::Path::new("/unused")).unwrap();
        let routes = Arc::new(ArcSwap::from_pointee(table));
        bind_until(&mut dp, listeners, &routes, &perf, 1).await;
        dp
    }

    /// An HTTPS listener deleted while a passthrough sibling keeps its port:
    /// the socket stays, and served the deleted listener's cert until
    /// restart. Each pass now plans the port from its listeners alone.
    #[tokio::test]
    async fn a_deleted_https_listener_leaves_no_cert_behind() {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        let Some(cert) = crate::proxy::tls::test_util::generate_test_cert("app.example.org") else {
            return; // openssl unavailable
        };
        let port = free_port();
        let https = https_with_cert(port, Some("app.example.org"), &cert);
        let passthrough = passthrough_listener(port, Some("secure.example.org"));
        let mut dp = serve(&[https, passthrough.clone()], RouteTable::new()).await;
        assert_eq!(
            presented(port, Some("app.example.org")).await,
            Ok(der(&cert.0))
        );

        let (perf, routes) = (
            test_perf(),
            Arc::new(ArcSwap::from_pointee(RouteTable::new())),
        );
        let pass = ensure(&mut dp, &[passthrough], &routes, &perf).await;
        assert_eq!(pass, (0, vec![]), "the socket stays");
        let after = presented(port, Some("app.example.org")).await;
        assert!(after.is_err(), "the deleted listener's cert: {after:?}");
    }

    /// A port that changes protocol keeps its socket and serves the new
    /// one from its next connection, HTTP to HTTPS and back. The socket kept
    /// the mode it was bound with until restart.
    #[tokio::test]
    async fn a_live_port_changes_protocol_without_a_rebind() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        let Some(cert) = crate::proxy::tls::test_util::generate_test_cert("app.example.org") else {
            return; // openssl unavailable
        };
        let port = free_port();
        let http = http_listener(port, None);
        let https = https_with_cert(port, None, &cert);
        let mut dp = serve(std::slice::from_ref(&http), RouteTable::new()).await;
        let (perf, routes) = (
            test_perf(),
            Arc::new(ArcSwap::from_pointee(RouteTable::new())),
        );
        let answers_http = || async {
            let mut client = tokio::net::TcpStream::connect(("127.0.0.1", port))
                .await
                .unwrap();
            client
                .write_all(b"GET / HTTP/1.1\r\nHost: h\r\nConnection: close\r\n\r\n")
                .await
                .unwrap();
            let mut response = Vec::new();
            let _ = client.read_to_end(&mut response).await;
            response.starts_with(b"HTTP/1.1 ")
        };
        assert!(answers_http().await);

        assert_eq!(ensure(&mut dp, &[https], &routes, &perf).await, (0, vec![]));
        assert_eq!(
            presented(port, Some("app.example.org")).await,
            Ok(der(&cert.0))
        );

        assert_eq!(ensure(&mut dp, &[http], &routes, &perf).await, (0, vec![]));
        assert!(answers_http().await);
    }

    /// No SNI, or one no listener names, goes to the port's listener without
    /// a hostname, whatever the listener order; with none, the connection
    /// closes unforwarded. The socket decided by which listener bound it.
    #[tokio::test]
    async fn the_catch_all_takes_no_sni_and_unmatched_sni_in_either_order() {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        let Some(cert) = crate::proxy::tls::test_util::generate_test_cert("app.example.org") else {
            return; // openssl unavailable
        };
        let in_order = |mut listeners: Vec<ListenerConfig>, reversed: bool| {
            if reversed {
                listeners.reverse();
            }
            listeners
        };
        for reversed in [false, true] {
            // An HTTPS catch-all beside a named passthrough listener.
            let (backend, forward) = test_backend().await;
            let port = free_port();
            let listeners = in_order(
                vec![
                    https_with_cert(port, None, &cert),
                    passthrough_listener(port, Some("secure.example.org")),
                ],
                reversed,
            );
            let mut table = RouteTable::new();
            table.add_tls_route(port, "secure.example.org", vec![forward]);
            let _dp = serve(&listeners, table).await;
            for sni in [None, Some("other.example.org")] {
                assert_eq!(presented(port, sni).await, Ok(der(&cert.0)), "{sni:?}");
            }
            let to_route = hello_reaches(port, Some("secure.example.org"), &backend).await;
            assert_eq!(to_route, Some(true));

            // A passthrough catch-all beside a named HTTPS listener: its
            // TCP route takes what no TLS route names.
            let (backend, forward) = test_backend().await;
            let port = free_port();
            let listeners = in_order(
                vec![
                    https_with_cert(port, Some("app.example.org"), &cert),
                    passthrough_listener(port, None),
                ],
                reversed,
            );
            let mut table = RouteTable::new();
            table.add_tcp_route(port, vec![forward]);
            let _dp = serve(&listeners, table).await;
            for sni in [None, Some("other.example.org")] {
                assert_eq!(
                    hello_reaches(port, sni, &backend).await,
                    Some(true),
                    "{sni:?}"
                );
            }
            assert_eq!(
                presented(port, Some("app.example.org")).await,
                Ok(der(&cert.0))
            );

            // No catch-all.
            let (backend, forward) = test_backend().await;
            let port = free_port();
            let listeners = in_order(
                vec![
                    https_with_cert(port, Some("app.example.org"), &cert),
                    passthrough_listener(port, Some("secure.example.org")),
                ],
                reversed,
            );
            let mut table = RouteTable::new();
            table.add_tls_route(port, "secure.example.org", vec![forward.clone()]);
            table.add_tcp_route(port, vec![forward]);
            let _dp = serve(&listeners, table).await;
            for sni in [None, Some("other.example.org")] {
                assert_eq!(hello_reaches(port, sni, &backend).await, None, "{sni:?}");
            }
        }
    }

    /// A backend the test holds, and a route's forward to it.
    async fn test_backend() -> (tokio::net::TcpListener, crate::routing::Backend) {
        let backend = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = backend.local_addr().unwrap().port();
        let forward = crate::routing::Backend::new("127.0.0.1".into(), port).unwrap();
        (backend, forward)
    }

    /// An HTTPS listener naming a host exactly takes it from a wildcard
    /// passthrough listener: exact before wildcard (GatewaySpec.listeners).
    /// Passthrough routes were tried first, across the port.
    #[tokio::test]
    async fn an_exact_https_listener_beats_a_wildcard_passthrough_one() {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        let Some(cert) = crate::proxy::tls::test_util::generate_test_cert("a.example.com") else {
            return; // openssl unavailable
        };
        let (backend, forward) = test_backend().await;
        let port = free_port();
        let listeners = [
            passthrough_listener(port, Some("*.example.com")),
            https_with_cert(port, Some("a.example.com"), &cert),
        ];
        let mut table = RouteTable::new();
        table.add_tls_route(port, "*.example.com", vec![forward]);
        let _dp = serve(&listeners, table).await;
        assert_eq!(
            presented(port, Some("a.example.com")).await,
            Ok(der(&cert.0))
        );
        assert_eq!(
            hello_reaches(port, Some("b.example.com"), &backend).await,
            Some(true)
        );
    }

    /// Regression: with unchanged certs, a reconcile that brings a NEW bind
    /// address must still bind it. The old flow `continue`d out of the whole
    /// listener iteration on a fingerprint match, silently skipping the
    /// bind loop for addresses that appeared after the first pass.
    #[tokio::test]
    async fn new_bind_address_with_unchanged_certs_still_binds() {
        let Some((cert, key)) = crate::proxy::tls::test_util::generate_test_cert("a.example.org")
        else {
            return; // openssl unavailable
        };
        let mut listener = https_listener("https", 23443, Some("a.example.org"), "cert");
        listener.tls.as_mut().unwrap().certificate_refs[0].cert_pem = Some(cert);
        listener.tls.as_mut().unwrap().certificate_refs[0].key_pem = Some(key);

        let on = |addresses: &[&str]| -> Vec<ListenerConfig> {
            addresses
                .iter()
                .map(|a| ListenerConfig {
                    address: Some(a.to_string()),
                    ..listener.clone()
                })
                .collect()
        };

        let perf = test_perf();
        let mut dp = DataPlane::new(&[], &perf, std::path::Path::new("/unused")).unwrap();
        let routes = Arc::new(ArcSwap::from_pointee(RouteTable::new()));

        let (opened, errors) = dp.add_tcp_listeners(&on(&["127.0.0.1"]), routes.clone(), &perf);
        assert_eq!((opened, errors.len()), (1, 0));
        let hash_after_first = dp
            .terminate_configs
            .get(&23443)
            .map(|(fingerprint, _)| *fingerprint);
        assert!(
            hash_after_first.is_some(),
            "TLS state prepared for the port"
        );

        // Same certs, one more address: the new endpoint must be bound and
        // the TLS state must not churn.
        let (opened, errors) =
            dp.add_tcp_listeners(&on(&["127.0.0.1", "127.0.0.2"]), routes, &perf);
        assert_eq!(errors, vec![]);
        assert_eq!(opened, 1, "the address that appeared later must be bound");
        assert_eq!(
            dp.terminate_configs
                .get(&23443)
                .map(|(fingerprint, _)| *fingerprint),
            hash_after_first
        );
    }

    /// Unparseable certs on a port shared by several Terminate listeners:
    /// one build attempt, one error, no listener bound on that port.
    #[tokio::test]
    async fn invalid_certs_error_once_per_port_and_skip_binding() {
        let listeners = vec![
            https_listener("a", 23444, None, "bad"),
            https_listener("b", 23444, Some("b.example.org"), "bad"),
        ];
        let perf = test_perf();
        let mut dp = DataPlane::new(&[], &perf, std::path::Path::new("/unused")).unwrap();
        let routes = Arc::new(ArcSwap::from_pointee(RouteTable::new()));

        let (opened, errors) = dp.add_tcp_listeners(&listeners, routes, &perf);
        assert_eq!(opened, 0, "a port that cannot handshake must not bind");
        assert_eq!(errors.len(), 1, "one error per port, not per listener");
        assert!(
            !dp.terminate_configs.contains_key(&23444),
            "failed build must retry next pass"
        );
    }

    fn http_listener(port: u16, address: Option<&str>) -> ListenerConfig {
        ListenerConfig {
            name: "http".into(),
            protocol: P::HTTP,
            port,
            target_port: None,
            hostname: None,
            address: address.map(str::to_string),
            interface: None,
            tls: None,
            rate_limit: None,
        }
    }

    fn free_port() -> u16 {
        test_ports::wildcard()
    }

    fn accepts(addr: &str, port: u16) -> bool {
        std::net::TcpStream::connect((addr, port)).is_ok()
    }

    /// One pass as the controller runs it: close what `listeners` no longer
    /// want, wait for those accept loops, bind the rest.
    async fn ensure(
        dp: &mut DataPlane,
        listeners: &[ListenerConfig],
        routes: &Arc<ArcSwap<RouteTable>>,
        perf: &crate::config::PerformanceConfig,
    ) -> (usize, Vec<(u16, String)>) {
        for accept_loop in dp.close_unwanted(listeners) {
            accept_loop.await.unwrap();
        }
        dp.add_tcp_listeners(listeners, routes.clone(), perf)
    }

    /// Moves `dp` to one listener as the controller does ([`ensure`]).
    /// Binds again while a test beside this one still holds the port, as
    /// `bind_until` does.
    async fn rebind(
        dp: &mut DataPlane,
        listener: &ListenerConfig,
        routes: &Arc<ArcSwap<RouteTable>>,
        perf: &crate::config::PerformanceConfig,
    ) {
        let listeners = std::slice::from_ref(listener);
        if ensure(dp, listeners, routes, perf).await.0 == 1 {
            return;
        }
        bind_until(dp, listeners, routes, perf, 1).await;
    }

    /// A listener that gains a specific address leaves the wildcard for
    /// it, and goes back when it loses it. The wildcard stayed bound while
    /// its port was still wanted, exposed on every interface, and counted
    /// as ready for the specific address, which was never bound.
    #[tokio::test]
    async fn wildcard_gives_way_to_a_specific_address_and_back() {
        // Accepting a probe builds a backend pool: rustls needs its provider.
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        let port = free_port();
        let on_every_interface = http_listener(port, None);
        let on_one_address = http_listener(port, Some("127.0.0.1"));
        let (wildcard, specific) = ((None, port), (Some("127.0.0.1".to_string()), port));
        let perf = test_perf();
        let mut dp = DataPlane::new(&[], &perf, std::path::Path::new("/unused")).unwrap();
        let routes = Arc::new(ArcSwap::from_pointee(RouteTable::new()));

        rebind(&mut dp, &on_every_interface, &routes, &perf).await;
        assert!(accepts("127.0.0.2", port));

        rebind(&mut dp, &on_one_address, &routes, &perf).await;
        assert!(dp.is_ready_for_endpoints(std::slice::from_ref(&specific)));
        assert!(!dp.is_ready_for_endpoints(std::slice::from_ref(&wildcard)));
        assert!(accepts("127.0.0.1", port));
        assert!(!accepts("127.0.0.2", port), "the wildcard is closed");

        rebind(&mut dp, &on_every_interface, &routes, &perf).await;
        assert!(dp.is_ready_for_endpoints(&[wildcard]));
        assert!(accepts("127.0.0.2", port));
    }

    /// A wildcard the spec wants stands for the specific addresses on its
    /// port, whatever the listener order: a specific bind beside it fails
    /// EADDRINUSE. The specific listener used to bind first and the
    /// wildcard then failed.
    #[tokio::test]
    async fn a_wanted_wildcard_stands_for_specific_addresses() {
        // Accepting a probe builds a backend pool: rustls needs its provider.
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        let port = free_port();
        let listeners = vec![
            http_listener(port, Some("127.0.0.1")),
            http_listener(port, None),
        ];
        let perf = test_perf();
        let mut dp = DataPlane::new(&[], &perf, std::path::Path::new("/unused")).unwrap();
        let routes = Arc::new(ArcSwap::from_pointee(RouteTable::new()));

        bind_until(&mut dp, &listeners, &routes, &perf, 1).await;
        let (opened, errors) = ensure(&mut dp, &listeners, &routes, &perf).await;
        assert_eq!((opened, errors), (0, vec![]), "nothing left to bind");
        assert!(dp.is_ready_for_endpoints(&[(None, port), (Some("127.0.0.1".to_string()), port)]));
        assert!(accepts("127.0.0.2", port));
    }

    /// A dead accept loop must drop out of readiness and be rebound by the
    /// next ensure pass. This is the ".21 outage" shape: the old add-only
    /// bookkeeping kept reporting bound (readiness green, Programmed=True)
    /// while the kernel refused every connection, until pod restart.
    #[tokio::test]
    async fn dead_accept_loop_unreadies_then_rebinds() {
        let listeners = vec![ListenerConfig {
            name: "http".into(),
            protocol: P::HTTP,
            port: 23447,
            target_port: None,
            hostname: None,
            address: None,
            interface: None,
            tls: None,
            rate_limit: None,
        }];
        let perf = test_perf();
        let mut dp = DataPlane::new(&[], &perf, std::path::Path::new("/unused")).unwrap();
        let routes = Arc::new(ArcSwap::from_pointee(RouteTable::new()));

        let (opened, errors) = dp.add_tcp_listeners(&listeners, routes.clone(), &perf);
        assert_eq!((opened, errors.len()), (1, 0));
        assert!(dp.is_ready_for_endpoints(&[(None, 23447)]));

        // Kill the accept loop out-of-band — stands in for any task death.
        if let Some(ListenerSlot::Running(handle, _)) = dp.listener_tasks.get(&(None, 23447)) {
            handle.abort();
        } else {
            panic!("expected a running listener slot");
        }
        // Bounded wait for the abort to land (is_finished flips async).
        for _ in 0..200 {
            if !dp.endpoint_alive(&(None, 23447)) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(
            !dp.is_ready_for_endpoints(&[(None, 23447)]),
            "a dead accept loop must not count as bound"
        );

        // The next ensure pass reaps the corpse and rebinds the endpoint.
        let (opened, errors) = ensure(&mut dp, &listeners, &routes, &perf).await;
        assert_eq!((opened, errors.len()), (1, 0), "dead endpoint must rebind");
        assert!(dp.is_ready_for_endpoints(&[(None, 23447)]));
    }

    /// Reconcile passes until `want` more listeners are bound, as the
    /// controller's next pass retries a failed bind. A port this process
    /// just closed can stay busy a moment: tests running beside this one
    /// hold references to its sockets (a spawned child until its exec, an
    /// fd scan's dups).
    async fn bind_until(
        dp: &mut DataPlane,
        listeners: &[ListenerConfig],
        routes: &Arc<ArcSwap<RouteTable>>,
        perf: &crate::config::PerformanceConfig,
        want: usize,
    ) {
        let mut opened = 0;
        for _ in 0..200 {
            opened += dp.add_tcp_listeners(listeners, routes.clone(), perf).0;
            if opened == want {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("{opened} of {want} listeners bound");
    }

    /// A listener removed from the Gateway spec must stop serving — the old
    /// add-only flow kept its port accepting until pod restart.
    #[tokio::test]
    async fn removed_listener_torn_down_and_port_released() {
        // A loopback address no other socket here binds or connects from,
        // and ports the OS hands out on it: neither this process's own
        // connects nor a run of this test in a parallel tree can take them.
        // Port 0 itself will not do, as listeners are keyed by port; both
        // probes are held until read, so the two differ.
        const ADDR: &str = "127.0.0.3";
        let probes = [(); 2].map(|()| std::net::TcpListener::bind((ADDR, 0)).unwrap());
        let [a, b] = probes.map(|probe| probe.local_addr().unwrap().port());
        let mk = |name: &str, port: u16| ListenerConfig {
            name: name.into(),
            protocol: P::HTTP,
            port,
            target_port: None,
            hostname: None,
            address: Some(ADDR.into()),
            interface: None,
            tls: None,
            rate_limit: None,
        };
        let endpoint = |port| (Some(ADDR.to_string()), port);
        let both = vec![mk("a", a), mk("b", b)];
        let perf = test_perf();
        let mut dp = DataPlane::new(&[], &perf, std::path::Path::new("/unused")).unwrap();
        let routes = Arc::new(ArcSwap::from_pointee(RouteTable::new()));

        bind_until(&mut dp, &both, &routes, &perf, 2).await;

        // Spec drops listener b: its endpoint must leave the map and the
        // socket must actually close (cancel lands async, poll briefly).
        let (opened, errors) = ensure(&mut dp, &both[..1], &routes, &perf).await;
        assert_eq!((opened, errors.len()), (0, 0));
        assert!(!dp.is_ready_for_endpoints(&[endpoint(b)]));
        assert!(dp.is_ready_for_endpoints(&[endpoint(a)]), "sibling kept");
        let mut released = false;
        for _ in 0..200 {
            if std::net::TcpListener::bind((ADDR, b)).is_ok() {
                released = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(released, "removed listener must release its port");

        // Re-adding the listener rebinds it.
        bind_until(&mut dp, &both, &routes, &perf, 1).await;
        assert!(dp.is_ready_for_endpoints(&[endpoint(b)]));
    }
}
