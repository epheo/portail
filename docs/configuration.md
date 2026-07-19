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
| `--access-log <PATH>` | Write one JSON line per completed HTTP response to PATH, `-` = stdout (off by default) |
| `--http2` | Enable HTTP/2 on TLS-terminate listeners (off by default; see HTTP/2 section) |
| `--config, -c <FILE>` | Load configuration from a JSON or YAML file (mutually exclusive with `--kubernetes`) |
| `--validate-only` | Validate config file without starting the server (requires `--config`) |
| `--check-config` | Parse and display config values, then exit (requires `--config`) |
| `--example-config` | Display paths to example configuration files and exit |
| `--generate-config <TYPE>` | Generate example config: `minimal` or `development` |
| `--output <FILE>` | Output file for generated configuration (requires `--generate-config`) |
| `--cert-dir <DIR>` | Directory for TLS certificate files (default: `/etc/portail/certs`) |
| `--supported-features` | Print supported Gateway API features and exit |
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
  (connections, backend pool, DNS refresh, health transitions, reconciles),
  `portail_http_requests_total{host=...}` and
  `portail_upstream_connect_{errors,timeouts}_total{backend=...}` families,
  per-listener `portail_listener_*{proto=...,port=...}` series including
  `portail_listener_up`, and two latency histograms:
  `portail_request_duration_seconds` (headers-complete to response flushed,
  upgrade tunnels excluded) and `portail_upstream_ttfb_seconds` (route match
  to backend response headers: connect, request forward, backend
  processing). Buckets are fixed, 10us to 60s.

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

## Performance Options

The `performance` block of a config file (all durations accept `"30s"`-style
values; defaults shown):

| Option | Default | Description |
|--------|---------|-------------|
| `backendTimeout` | `30s` | Backend connect timeout |
| `clientHeaderTimeout` | `30s` | Max time for a client TLS handshake, or to finish a request's header block once its first bytes arrive (slow-loris guard). Does not bound idle keepalive waits |
| `idleBodyTimeout` | `300s` | Per-step progress budget on request/response body streaming; reaps peers that stay connected but stop transferring. `0s` disables |
| `tcpKeepaliveTime` | `60s` | Idle time before the kernel probes an accepted client socket; reaps peers that vanished without FIN/RST before they pin fds forever |
| `udpSessionTimeout` | `30s` | Idle expiry for per-client UDP sessions |
| `dnsRefreshInterval` | `5s` | How often backend FQDNs are re-resolved and the route table swapped on change (STRICT_DNS freshness window) |
| `backendPoolScope` | `connection` | Idle backend-connection pool scope: `connection` pools per client connection (lock-free); `process` shares one pool across all clients (cross-client reuse under churn, at the cost of a sharded lock) |
| `http2` | `false` | Serve HTTP/2 on TLS-terminate listeners (same switch as `--http2`; see HTTP/2 section) |

In Kubernetes mode these are not read from a file; the defaults apply.

## HTTP/2

Opt-in via `--http2` (Kubernetes mode) or `performance.http2: true`
(standalone). When enabled, TLS-terminate listeners advertise `h2` ahead of
`http/1.1` via ALPN; the client's negotiation picks the protocol, so h1 and
h2 clients share the same port. Plain-HTTP and passthrough listeners are
unaffected, and with the flag off the data path is byte-for-byte unchanged.

Each h2 stream is bridged through the same HTTP/1.1 engine that serves h1
clients — routing, filters, timeouts, backend pooling, metrics, and access
logs behave identically. The bridge costs one in-memory hop per direction,
paid only by h2 connections. Negotiation volume is visible in
`portail_http2_connections_total`.

Limits to know about:

- Trailers do not survive the bridge, so gRPC cannot ride it; GRPCRoute
  support requires the future native h2 path.
- Each h2 stream is its own engine exchange. With the default
  `backendPoolScope: connection` there is no backend reuse across streams;
  set `backendPoolScope: process` alongside `http2` for backend keepalive.
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

The default cert directory is `/etc/portail/certs`.
