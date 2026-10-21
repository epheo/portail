# Configuration

## CLI Reference

| Flag | Description |
|------|-------------|
| `--kubernetes` | Run as Kubernetes Gateway API controller |
| `--controller-name <NAME>` | Controller name for GatewayClass matching (default: `portail.epheo.eu/gateway-controller`) |
| `--gateway <NS/NAME>` | Restrict the controller to a single Gateway; set by portail-operator on per-Gateway data planes. Absent = watch all Gateways cluster-wide |
| `--watch-shape <TOKENS>` | Operator-set watch narrowing (comma tokens: `tls`, `ns-labels`); absent watches all secondary resources |
| `--manage-gateway-status <BOOL>` | Manage Gateway lifecycle status (Accepted/Programmed/addresses). Set `false` under portail-operator, which owns those; portail then reports only listener and route status (default: `true`) |
| `--readiness-port <PORT>` | Port for the `/livez` + `/readyz` + `/metrics` admin endpoint in Kubernetes mode (default: `19099`) |
| `--metrics-port <PORT>` | Serve the same admin endpoint in standalone mode (off by default) |
| `--admin-address <IP>` | Bind address of the admin endpoint, both modes (default: `0.0.0.0`, every interface). Kubelet probes reach the pod IP, so loopback would fail readiness; the pod IP keeps `/metrics` off public VIP and Multus interfaces |
| `--access-log <PATH>` | Write one JSON line per completed HTTP response to PATH, `-` = stdout (off by default) |
| `--http2` | Enable HTTP/2 on TLS-terminate listeners (off by default; see HTTP/2 section) |
| `--config, -c <FILE>` | Load configuration from a JSON or YAML file (mutually exclusive with `--kubernetes`) |
| `--validate-only` | Validate config file without starting the server (requires `--config`) |
| `--check-config` | Parse and display config values, then exit (requires `--config`) |
| `--example-config` | List the built-in example configurations and exit |
| `--generate-config <TYPE>` | Generate example config: `minimal` or `development` |
| `--output <FILE>` | Output file for generated configuration (requires `--generate-config`) |
| `--cert-dir <DIR>` | Directory for TLS certificate files (default: `certs`, relative to the working directory) |
| `--supported-features` | Print supported Gateway API features and exit |
| `--print-crd` | Print portail's CRD manifests (RateLimitPolicy) as YAML and exit |
| `--verbose, -v` | Enable verbose logging (repeat for more: `-vv`, `-vvv`) |

## Environment Variables

| Variable | Description |
|----------|-------------|
| `PORTAIL_WORKER_THREADS` | Tokio worker threads. Default: `min(available CPUs, 4)` — sized to the pod, not the node, so many per-Gateway pods can pack onto one node without exhausting its thread budget. Raise for high-traffic or standalone use |
| `PORTAIL_MAX_BLOCKING_THREADS` | Tokio blocking-thread cap (default: `32`) |

## Admin Endpoint

One plain-HTTP management server, on `--readiness-port` (default `19099`) in
Kubernetes mode or opt-in via `--metrics-port` in standalone mode:

- `GET /livez` — always `200` once the process is up. Liveness only: "should
  this process be restarted", never "is it ready".
- `GET /readyz` (any non-metrics path) — `200` once the data plane has bound
  its listener ports, `503` before. Gate traffic on this.
- `GET /metrics` — Prometheus text format. Process-wide counters
  (connections, backend pool, DNS refresh, health transitions, reconciles,
  and `portail_http_requests_rejected_total` for request heads refused with
  `400` as malformed or ambiguous; those are logged at debug level only),
  `portail_http_requests_total{host=...}`,
  `portail_upstream_connect_{errors,timeouts}_total{backend=...}` and
  `portail_upstream_send_errors_total{backend=...}` (a request a freshly
  connected backend did not take) families,
  per-listener `portail_listener_*{proto=...,port=...}` series including
  `portail_listener_up`, and two latency histograms:
  `portail_request_duration_seconds` (headers-complete to response flushed,
  upgrade tunnels excluded) and `portail_upstream_ttfb_seconds` (route match
  to backend response headers: connect, request forward, backend
  processing). Buckets are fixed, 10us to 60s.

The endpoint listens on every interface unless `--admin-address` names one.
On a pod carrying a Gateway VIP or a Multus interface, the pod IP keeps
kubelet probes working while those interfaces stop exposing `/metrics`:

```yaml
args: ["--admin-address=$(POD_IP)"]
env:
- name: POD_IP
  valueFrom:
    fieldRef:
      fieldPath: status.podIP
```

## Access Logs

`--access-log <PATH>` (`-` = stdout) writes one JSON line per completed HTTP
response, in both Kubernetes and standalone mode:

```json
{"ts":1752822334.123,"client":"192.0.2.7","listener":443,"tls":true,"method":"GET","path":"/api","host":"api.example.com","status":200,"backend":"10.1.2.3:8080","duration_us":1234}
```

`host` is the request's Host header; `backend` is `null` for responses the
proxy answered itself (redirects, direct errors); `duration_us` runs from
headers-complete to the response leaving the proxy. Method, path, and host
are attacker-controlled and JSON-escaped. The log never slows the data
path: lines go to a dedicated writer through a bounded queue, and a sink
slower than the request rate sheds lines, counted in
`portail_access_log_dropped_total`. Requests aborted mid-response (client
gone, backend died with headers already sent) produce no line.

## Request Paths

Routes match the request path with its dot-segments removed (RFC 3986
5.2.4), and the backend gets that same path, over HTTP/1 and HTTP/2 alike.
A backend resolving `/public/../admin` to `/admin` is then reached through
the route on `/admin`, never past it:

- `.` and `..` segments are resolved; `%2e` and `%2E` count as a dot.
- A `..` above the root stops there: `/../a` becomes `/a`.
- Nothing else changes: empty segments stay (`//` is not merged), no
  other escape is decoded (`%2F` stays as sent), the query is untouched.
- Access logs keep the path as the client sent it.

URLRewrite and RequestRedirect path modifiers replace the path only. The
query string is kept as sent, in the forwarded request and in a
redirect's `Location`.

## Rate Limiting

Per-client token bucket, attached per listener. Clients are keyed by source
IPv4 address or IPv6 /64. A request finding the bucket empty is answered
`429` with `Retry-After` before any backend is touched; the connection is
kept open when safe so a throttled client cannot force TLS re-handshakes.
An unlimited listener costs the hot path one null check per request.

Kubernetes mode — a `RateLimitPolicy` attached to a Gateway (install the CRD
from `examples/kubernetes/crds/ratelimitpolicy.yaml`, printable via
`--print-crd`):

```yaml
apiVersion: portail.epheo.eu/v1alpha1
kind: RateLimitPolicy
metadata:
  name: edge-clients
  namespace: portail-system
spec:
  targetRef:
    group: gateway.networking.k8s.io
    kind: Gateway
    name: public
    # sectionName: https-web   # limit one listener instead of the Gateway
  requestsPerSecond: 2   # sustained per-client budget (bucket refill)
  burst: 60              # one-shot budget (bucket size); default = requestsPerSecond
```

A `sectionName`-scoped policy overrides a Gateway-wide one for its listener;
among policies of equal scope the oldest wins (GEP-713 conflict rule) and
later ones report `Conflicted` in `status.ancestors`. The policy must live
in the Gateway's namespace. Without the CRD installed, nothing changes.
A CRD installed later is watched within ten minutes, without a restart;
the same holds for TCPRoute, TLSRoute and UDPRoute.

Standalone mode — the same parameters on a listener:

```yaml
gateway:
  listeners:
    - name: web
      protocol: HTTP
      port: 8080
      rate_limit:
        requestsPerSecond: 2
        burst: 60
```

Listener rate limits apply to routes bound to that listener via
`parentRefs` (like hostname scoping); routes without `parentRefs` bypass
listener scoping and are never limited. Policy edits keep per-client state:
a throttled crawler stays throttled across a rate change. Observability:
`portail_rate_limited_total`, `portail_listener_rate_limited_total`, and
`status: 429` access-log lines. A limited request counts there only:
`portail_http_requests_total` and `portail_listener_http_requests_total`
count requests dispatched to a backend.

## Route Attachment

A route is served on exactly the listeners its status attaches it to.

Kubernetes mode:

- A parentRef attaches to the listeners it selects (by `sectionName`, `port`, or all) that admit the route: `allowedRoutes` namespaces and kinds, hostname intersection, and a protocol portail serves.
- A parentRef without `namespace` names a Gateway in the route's own namespace.
- Equally specific matches go to the oldest route (`creationTimestamp`), then to the first by namespace/name.
- A TCP or UDP listener carries one route, the oldest. A parentRef whose listeners are all taken reports `Accepted=False` with reason `Conflicted`.
- TCPRoute and UDPRoute rules have no match to choose between, so only the first rule is used.
- An invalid backendRef (`RefNotPermitted`, `BackendNotFound`, `InvalidKind`) keeps its weight. Its share of HTTP requests gets `500`; its share of TCP and TLS passthrough connections is closed, of UDP datagrams dropped.

Standalone mode:

- A parentRef without `sectionName` or `port` selects every listener, for TCP and UDP routes too.
- A TCP route can also attach to a TLS listener, as the fallback for passthrough connections no TLS route matches.
- Where routes tie (equally specific matches, a shared TCP or UDP listener), the first in the file wins.
- TCP and UDP routes use their first rule only.

## HTTPRoute Rules

Kubernetes HTTPRoutes and the `httpRoutes` of a config file are checked by the same rules.

Matching:

- `RegularExpression` path, header and query matches use the Rust `regex` crate syntax (RE2-like: no look-around, no backreferences).
- A regex must match the whole value: path regex `/api` matches `/api`, not `/static/api/x`.
- `PathPrefix` ignores a trailing `/`: `/abc/` matches `/abc`, `/abc/` and `/abc/def`, not `/abcd`.
- A rule with no `matches`, or `matches: []`, matches every request, like `PathPrefix /`.

Filters:

- Only `RequestMirror` may appear more than once in one filter list.
- `RequestRedirect` and `URLRewrite` cannot share a rule.
- A `URLRewrite` hostname and a `RequestHeaderModifier` that sets `Host` cannot share a rule.
- `ReplacePrefixMatch` needs every match of its rule to be a `PathPrefix`.
- A backendRef filter must be a `RequestHeaderModifier` or `ResponseHeaderModifier`.
- `RequestMirror` keeps its share exact: `fraction: {numerator: 1, denominator: 1000}` mirrors one request in a thousand.
- `percent` must be 0 to 100; a `fraction` needs a denominator of at least 1 and a numerator no larger.

A route that breaks one of these rules:

- Kubernetes: it is rejected alone, `Accepted=False`, and every other route keeps serving.
- The reason is `UnsupportedValue` for a value portail cannot apply, `IncompatibleFilters` for filters that cannot share a rule.
- Standalone: the config fails to load; a SIGHUP reload keeps the running routes.

A backend or mirror name DNS cannot resolve:

- Kubernetes: the backend is left out and its share goes to the other backends; a mirror is dropped.
- The DNS refresh (`dnsRefreshInterval`) brings it back once it resolves.
- Standalone: the config fails to load; a SIGHUP reload keeps the running routes.

## Backend Services

In Kubernetes mode, HTTP, TCP, UDP and TLS passthrough backends and `RequestMirror` all reach a Service the same way:

- ClusterIP: its DNS name, on the Service port, which kube-proxy maps to the pods.
- Headless: every ready pod DNS returns, on the `targetPort`, as DNS carries no port. A named `targetPort` is read from the Service's EndpointSlices.
- ExternalName: its external name, on the Service port.
- A mirror sends its copies to one address: the smallest the Service resolves to.

## Performance Options

The `performance` block of a config file (all durations accept `"30s"`-style
values; defaults shown):

| Option | Default | Description |
|--------|---------|-------------|
| `backendTimeout` | `30s` | Backend connect timeout |
| `clientHeaderTimeout` | `30s` | Slow-loris guard: max time for a TLS handshake or a request's header block |
| `idleBodyTimeout` | `300s` | Reaps peers that stay connected but stop transferring |
| `tcpKeepaliveTime` | `60s` | Idle time before the kernel probes an accepted client socket or a backend socket; reaps peers that vanished without FIN/RST before they pin fds forever |
| `udpSessionTimeout` | `30s` | Idle expiry for per-client UDP sessions; must be above `0s` |
| `udpMaxSessions` | soft `RLIMIT_NOFILE` / 4, shared | Concurrent UDP sessions per listener socket |
| `dnsRefreshInterval` | `5s` | How often backend FQDNs are re-resolved and the route table swapped on change (STRICT_DNS freshness window). In Kubernetes mode a change also re-reads the named `targetPort`s of headless backends |
| `backendPoolScope` | `connection` | Idle backend-connection pool scope: `connection` pools per client connection (lock-free); `process` shares one pool across all clients (cross-client reuse under churn, at the cost of a sharded lock) |
| `http2` | `false` | Serve HTTP/2 on TLS-terminate listeners (same switch as `--http2`; see HTTP/2 section) |

In Kubernetes mode these are not read from a file; the defaults apply.

`clientHeaderTimeout`:

- The header budget starts at a request's first bytes.
- Idle keepalive waits stay unbounded.
- It also bounds each connection's close, TLS close_notify included.
- A closing reply lingers: it reads off what the client still sends, so no reset destroys the reply.
- Unless the client is done: it asked to close, its request is all in, and nothing came past it.
- These closes never linger: a 408, a 503 for the proxy's own fd or memory shortage, a tunnel's end, a reset after a response cut short.
- The linger lasts `clientHeaderTimeout` at most.
- It stops after 5s with nothing new from the client.
- It stops after 64KiB, one client buffer.
- An HTTP/2 connection's close always lingers, within the same bounds.

`idleBodyTimeout`:

- Each step of request and response body streaming must progress within it.
- So must an L4 relay once one direction has closed.
- It also bounds a stalled h2 connection: writes pending that long drop it.
- Idle keepalive waits and idle h2 connections stay unbounded.
- `0s` disables.

`udpMaxSessions`:

- A datagram that would open one more session is dropped.
- Drops are counted in `portail_udp_session_overflow_drops_total`.
- Each session holds an fd, a task, a 64KiB reply buffer and an ephemeral port.
- Spoofed source addresses cost nothing to send from.
- Unset, all UDP listeners share one budget, a quarter of the fd limit.
- The other three quarters stay for TCP.
- The fd limit is read after startup raises the soft limit to the hard one.
- The default scales with it, as Kubernetes mode cannot set this option.
- A fixed default would cut off UDP workloads larger than it.
- The shared budget lets a flood on one UDP listener take every session slot.
- Set `udpMaxSessions` to isolate listeners: each gets that many slots of its own.
- Their caps then add up: keep the sum within a quarter of the fd limit.

## HTTP/2

Opt-in via `--http2` (Kubernetes mode) or `performance.http2: true`
(standalone). When enabled, TLS-terminate listeners advertise `h2` ahead of
`http/1.1` via ALPN; the client's negotiation picks the protocol, so h1 and
h2 clients share the same port. Plain-HTTP and passthrough listeners are
unaffected, and with the flag off the data path is byte-for-byte unchanged.

Each h2 stream is bridged through the same HTTP/1.1 engine that serves h1
clients - routing, filters, timeouts, backend pooling, metrics, and access
logs behave identically. The bridge costs one in-memory hop per direction,
paid only by h2 connections. Negotiation volume is visible in
`portail_http2_connections_total`.

The streams of one h2 connection share one backend pool, like keepalive requests on an h1 connection.
`backendPoolScope: process` extends that reuse across clients.

Limits to know about:

- Trailers do not survive the bridge, so gRPC cannot ride it; GRPCRoute
  support requires the future native h2 path.
- WebSocket/CONNECT streams are refused over h2; h2-capable browsers fall
  back to HTTP/1.1 for WebSocket automatically.

## Custom Controller Name

To use a custom controller name (e.g., for running multiple Portail instances):

```bash
portail --kubernetes --controller-name my-org.example.com/gateway-controller
```

Update the `GatewayClass` to match:

```yaml
apiVersion: gateway.networking.k8s.io/v1
kind: GatewayClass
metadata:
  name: portail
spec:
  controllerName: my-org.example.com/gateway-controller
```

## Standalone Mode

Portail can run without Kubernetes, loading routes from a config file:

```bash
# Generate a starter config
portail --generate-config development --output config.yaml

# Validate without running
portail --config config.yaml --validate-only

# Inspect parsed config
portail --config config.yaml --check-config

# Run with config file
portail --config config.yaml
```

## TLS Certificates

### Kubernetes Mode

Portail reads TLS certificates from Kubernetes Secrets referenced in your Gateway listener's `certificateRefs`. It works with cert-manager or any other Secret-based certificate management.

TLS certificates are hot-reloaded when the underlying Secret changes — no restart required. Multiple Gateways sharing a port have their certificates merged automatically for SNI-based selection.

Example with cert-manager:

```yaml
apiVersion: cert-manager.io/v1
kind: Certificate
metadata:
  name: app-tls
  namespace: default
spec:
  secretName: app-tls-secret
  issuerRef:
    name: letsencrypt
    kind: ClusterIssuer
  dnsNames:
  - app.example.com
---
apiVersion: gateway.networking.k8s.io/v1
kind: Gateway
metadata:
  name: secure-gateway
  namespace: default
spec:
  gatewayClassName: portail
  listeners:
  - name: https
    port: 443
    protocol: HTTPS
    hostname: "app.example.com"
    tls:
      mode: Terminate
      certificateRefs:
      - name: app-tls-secret
```

### Standalone Mode

In standalone mode, TLS certificates are read from the filesystem. Place `{name}.crt` and `{name}.key` files in the cert directory:

```bash
portail --config config.yaml --cert-dir /path/to/certs
```

The default cert directory is `certs`, relative to the working directory.
