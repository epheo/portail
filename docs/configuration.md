# Configuration

## CLI Reference

One binary, three modes:

- `portail controller`: the Kubernetes controller.
- `portail dataplane`: one Gateway's data plane, fed by the controller.
- `portail` without a subcommand: standalone, from `--config` or the built-in default config.

### Standalone

| Flag | Description |
|------|-------------|
| `--config, -c <FILE>` | Load configuration from a JSON or YAML file |
| `--validate-only` | Validate config file without starting the server (requires `--config`) |
| `--check-config` | Parse and display config values, then exit (requires `--config`) |
| `--example-config` | List the built-in example configurations and exit |
| `--generate-config <TYPE>` | Generate example config: `minimal` or `development` |
| `--output <FILE>` | Output file for generated configuration (requires `--generate-config`) |
| `--cert-dir <DIR>` | Directory for TLS certificate files (default: `certs`, relative to the working directory) |
| `--metrics-port <PORT>` | Serve the `/livez` + `/readyz` + `/metrics` admin endpoint (off by default) |
| `--admin-address <IP>` | Bind address of the admin endpoint (default: `0.0.0.0`, every interface) |
| `--access-log <PATH>` | Write one JSON line per completed HTTP response to PATH, `-` = stdout (off by default) |
| `--http2` | Enable HTTP/2 on TLS-terminate listeners (off by default; see HTTP/2 section) |
| `--supported-features` | Print supported Gateway API features and exit |
| `--print-crd` | Print portail's CRD manifests (GatewayParameters, RateLimitPolicy) as YAML and exit |
| `--verbose, -v` | Enable verbose logging (repeat for more: `-vv`, `-vvv`); also accepted before a subcommand |

These flags take no subcommand: `portail --http2 dataplane ...` is refused.

### `portail controller`

| Flag | Description |
|------|-------------|
| `--controller-name <NAME>` | Controller name for GatewayClass matching (default: `portail.epheo.eu/gateway-controller`) |
| `--namespace <NS>` | Namespace of the leader election Lease, the CA Secret and ConfigMap, and the Service (default: env `POD_NAMESPACE`) |
| `--service-name <NAME>` | The ClusterIP Service data planes dial; `<service>.<namespace>.svc` is also the audience of their tokens (default: `portail-controller`) |
| `--sync-port <PORT>` | Port of the TLS sync endpoint, `POST /v1/sync/<namespace>/<name>` (default: `9443`) |
| `--sync-peer-connections <N>` | Open sync connections one peer IP may hold, held polls aside (default: `32`). A held poll counts against its Gateway, not its address, so data planes polling through the apiserver's proxy, which all share one, do not crowd each other out |
| `--sync-gateway-polls <N>` | Polls one Gateway's data planes may hold at once (default: `8`; each pod holds one). Over it a poll is answered 503 at once: pods that a namespace's admin starts on its Gateway's ServiceAccount never take other Gateways' connections. Held polls still take the controller's 1024 connections, so 128 Gateways at their cap fill it |
| `--leader-election-id <NAME>` | Name of the leader election Lease (default: `portail-controller`); the Role of `deploy/install` grants this name only |
| `--dataplane-image <IMAGE>` | Data-plane image of Managed Gateways without a GatewayParameters `image` (default: the image this binary was published as) |
| `--readiness-port <PORT>` | Port of the admin endpoint (default: `19099`) |
| `--admin-address <IP>` | Bind address of the admin endpoint (default: `0.0.0.0`) |

### `portail dataplane`

| Flag | Description |
|------|-------------|
| `--gateway <NS/NAME>` | The Gateway this data plane serves (required) |
| `--controller-address <URL>` | The controller's sync endpoint, e.g. `https://portail-controller.portail-system.svc:9443` (required) |
| `--controller-ca-file <FILE>` | PEM of the CA the controller's serving certificate chains to (default: the PEM in env `PORTAIL_CONTROLLER_CA`) |
| `--token-file <FILE>` | Projected ServiceAccount token presented to the controller, re-read per request (default: `/var/run/secrets/portail.epheo.eu/controller/token`) |
| `--transport <direct\|apiserver-proxy>` | How to reach the controller (default: `direct`). `apiserver-proxy` polls through the apiserver's service proxy, to the Service `--controller-address` names (`<service>.<namespace>.svc`), for pods that reach only the apiserver: see [Primary UDN](deployment.md#primary-udn) |
| `--apiserver-address <URL>` | With `apiserver-proxy`: the apiserver (default: `https://$KUBERNETES_SERVICE_HOST:$KUBERNETES_SERVICE_PORT`); its certificate must name `kubernetes.default.svc` |
| `--apiserver-token-file <FILE>` | With `apiserver-proxy`: a projected ServiceAccount token of the API audience, re-read per request (default: `/var/run/secrets/portail.epheo.eu/apiserver/token`) |
| `--apiserver-ca-file <FILE>` | With `apiserver-proxy`: the CA the apiserver's certificate chains to, the only one trusted (default: `/var/run/secrets/portail.epheo.eu/apiserver/ca.crt`) |
| `--shutdown-delay <DURATION>` | On SIGTERM, keep serving this long before draining (default: `5s`) |
| `--readiness-port <PORT>` | Port of the admin endpoint (default: `19099`) |
| `--admin-address <IP>` | Bind address of the admin endpoint (default: `0.0.0.0`). Kubelet probes reach the pod IP, so loopback would fail readiness; the pod IP keeps `/metrics` off public VIP and Multus interfaces |
| `--access-log <PATH>` | As in standalone mode |
| `--http2` | As in standalone mode |

Managed data planes run with `--gateway`, `--controller-address`, `--admin-address=$(POD_IP)`, `--shutdown-delay=0s` under a `Recreate` rollout, and `--transport apiserver-proxy` under GatewayParameters `controllerTransport: APIServerProxy`, with the API token and `kube-root-ca.crt` mounted at the `--apiserver-*-file` defaults.
Managed data planes get `--access-log=-` under GatewayParameters `accessLog: Stdout`, and `--http2` under `http2: Enabled`; an Unmanaged data plane's owner passes them.

## Environment Variables

| Variable | Description |
|----------|-------------|
| `PORTAIL_WORKER_THREADS` | Tokio worker threads. Default: `min(available CPUs, 4)`, sized to the pod, not the node, so many per-Gateway pods can pack onto one node without exhausting its thread budget. Managed data planes get `min(CPU limit, 4)`. Raise for high-traffic or standalone use |
| `PORTAIL_MAX_BLOCKING_THREADS` | Tokio blocking-thread cap (default: `32`) |
| `POD_NAMESPACE` | The controller's `--namespace` default |
| `PORTAIL_CONTROLLER_CA` | The data plane's controller CA, as PEM, when `--controller-ca-file` is not given |
| `POD_IP` | The data plane's pod IP, `status.podIP` through the downward API (Managed pods get it): while a network it only dials out on is unresolved, a named address binds only on this IP's interface or a listening Network's |
| `KUBERNETES_SERVICE_HOST`, `KUBERNETES_SERVICE_PORT` | Set by kubelet: the data plane's `--apiserver-address` default with `--transport apiserver-proxy` |

## Admin Endpoint

One plain-HTTP management server, on `--readiness-port` (default `19099`) in
`portail controller` and `portail dataplane`, or opt-in via `--metrics-port`
in standalone mode:

- `GET /livez`: always `200` once the process is up. Liveness only: "should
  this process be restarted", never "is it ready".
- `GET /readyz` (any non-metrics path): `200`, else `503`. A data plane: once
  it applied a configuration and bound every listener of it; it stays `200`
  through `--shutdown-delay` after SIGTERM. Standalone: once its listener
  ports are bound. The controller: once its caches synced and its CA loaded,
  standbys included.
- `GET /metrics`: Prometheus text format. Process-wide counters
  (connections, backend pool, DNS refresh, health transitions, reconciles,
  and `portail_http_requests_rejected_total` for requests refused with
  `400` before any backend, their head or chunked body framing malformed or
  ambiguous; those are logged at debug level only),
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
- A data plane adds `portail_controller_connected` (`0` while it cannot reach
  the controller) and `portail_config_age_seconds`: alert on the age, since
  route and certificate changes wait for the controller.
- The controller adds `portail_sync_auth_rejected_total{stage}` (`precheck`,
  `negative_cache`, `signature`, `tokenreview`, `authz`, `serviceaccount`),
  `portail_tokenreview_requests_total{result}`,
  `portail_sync_throttled_total` (503s: TokenReviews in flight at their
  cap, or a source IP past 5 reviews a second of tokens not seen yet) and
  `portail_sync_auth_stale_verdicts_total` (polls served on an expired
  TokenReview verdict while the API server did not answer its renewal).

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
response, in a data plane or standalone:

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
- A `.` or `..` segment with path parameters (`/public/..;/admin`) gets
  `400` and closes the connection, counted in
  `portail_http_requests_rejected_total`: Tomcat, Jetty and Spring drop
  the parameters and resolve it, RFC 3986 does not. Parameters on any
  other segment (`/a;jsessionid=1`) pass.
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

Kubernetes mode: a `RateLimitPolicy` attached to a Gateway (`deploy/install`
installs the CRD, `deploy/crds/ratelimitpolicy.yaml`, also printed by
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
A CRD installed later is watched without a restart, within ten minutes:
API discovery is read every ten minutes while a CRD is missing.
The same holds for TCPRoute, TLSRoute and UDPRoute.

Standalone mode: the same parameters on a listener:

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

## PROXY Protocol

The receiving half of PROXY protocol v2, for a Gateway behind a load balancer or edge proxy that sends it.

Kubernetes mode: `proxyProtocol.trustedSources` in GatewayParameters, CIDRs or Gateways ([deployment](deployment.md#proxy-protocol)).
Standalone mode:

```yaml
gateway:
  proxyProtocol:
    trustedSources: ["10.250.0.0/16", "2001:db8:250::/48"]
```

- A connection from a trusted source must open with a v2 header, on every TCP listener, plain or TLS.
- PROXY for a TCP4 or TCP6 client: that client stands for the connection: `X-Forwarded-For`, access logs, rate limits.
- LOCAL, a health check: the connection's own address stands.
- Anything else from a trusted source closes the connection, never falling back to its address: no header within `clientHeaderTimeout`, a malformed one, v1 text, a UDP or unix client.
- Counted in `portail_proxy_protocol_refused_total`.
- Other sources are never read for a header: one they send is bytes of their request.
- TLVs are skipped. UDP listeners take no header.
- CIDRs name networks: `10.250.0.7/16` is refused, write `10.250.0.0/16` or `10.250.0.7/32`.
- The sending half goes to backendRefs of kind Gateway, in Kubernetes mode ([deployment](deployment.md#gateway-backends)), and to a Service port whose `appProtocol` is `portail.epheo.eu/proxy-v2` ([deployment](deployment.md#proxy-protocol-to-a-service)) or a standalone backendRef whose `app_protocol` is.

## Route Attachment

A route is served on exactly the listeners its status attaches it to.

Kubernetes mode:

- A parentRef attaches to the listeners it selects (by `sectionName`, `port`, or all) that admit the route: `allowedRoutes` namespaces and kinds, hostname intersection, and a protocol portail serves.
- A parentRef without `namespace` names a Gateway in the route's own namespace.
- A `Conflicted` listener, not distinct from another on its port, is not served: nothing binds, no route attaches, and a parentRef naming it reports `NoMatchingParent`.
- HTTPS and TLS listeners can share a port if their hostnames differ: one socket serves both, by SNI (see [Shared TLS Ports](#shared-tls-ports)). Any other two protocols on one port are `ProtocolConflict`.
- A TLS listener is served in `Passthrough` mode only: TLSRoute is passthrough here, and an HTTPS listener terminates. A TLS listener in `Terminate` mode reports `Accepted=False` with reason `UnsupportedProtocol`, binds nothing and takes no route.
- A listener portail does not serve (an unsupported protocol, TLS `Terminate`) conflicts with no other: it is rejected alone.
- Equally specific matches go to the oldest route (`creationTimestamp`), then to the first by the string `{namespace}/{name}`: `a-b/c` comes before `a/z`.
- A TCP or UDP listener carries one route, the oldest. A parentRef whose listeners are all taken is `Accepted` and counted in their `attachedRoutes`, but gets no traffic (Gateway API v1.7).
- TCPRoute and UDPRoute rules have no match to choose between, so only the first rule is used.
- An invalid backendRef (`RefNotPermitted`, `BackendNotFound`, `InvalidKind`) keeps its weight. Its share of HTTP requests gets `500`; its share of TCP and TLS passthrough connections is closed, of UDP datagrams dropped.

Standalone mode:

- A parentRef without `sectionName` or `port` selects every listener, for TCP and UDP routes too.
- An HTTP route attaches to HTTP and HTTPS listeners only: on a TCP port it would hold back server-speaks-first clients.
- A TCP route can also attach to a TLS listener, as the fallback for passthrough connections no TLS route matches.
- Where routes tie (equally specific matches, a shared TCP or UDP listener), the first in the file wins.
- An HTTP route without parentRefs (a config built without validation) serves every HTTP and HTTPS port, ranked with the routes of the port's listener without a hostname: the more specific hostname, then the more specific match, then the first in the file.
- TCP and UDP routes use their first rule only.
- Listeners on one port share its routes, whatever address each binds: HTTP, TCP, and HTTPS with TLS are three families, and two families on one port fail the load (`Protocol conflict`).

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
- A `RequestRedirect` rule never forwards: `backendRefs` beside it are unused. The HTTPRoute CRD refuses them; a config file loads with a warning.
- Those unused `backendRefs` are never resolved, health-probed, validated or counted in `ResolvedRefs`.
- A `URLRewrite` hostname and a `RequestHeaderModifier` that sets `Host` cannot share a rule.
- `ReplacePrefixMatch` needs every match of its rule to be a `PathPrefix`.
- A path modifier replaces the path only: the request keeps its query. A `replaceFullPath` or `replacePrefixMatch` value with a query of its own puts it first: `/login?next=home` on `/old?a=1` gives `/login?next=home&a=1`.
- The `&` goes in only when both queries are non-empty and the value's does not already end in `&`: `/login?next=home&` on `/old?a=1` gives `/login?next=home&a=1`.
- A backendRef filter must be a `RequestHeaderModifier` or `ResponseHeaderModifier`.
- `RequestMirror` keeps its share exact: `fraction: {numerator: 1, denominator: 1000}` mirrors one request in a thousand.
- `percent` must be 0 to 100; a `fraction` needs a denominator of at least 1 and a numerator no larger.
- A `RequestMirror` sets `percent` or `fraction`, not both.

A route that breaks one of these rules:

- Kubernetes: it is rejected alone, `Accepted=False`, and every other route keeps serving.
- The reason is `UnsupportedValue` for a value portail cannot apply, `IncompatibleFilters` for filters that cannot share a rule.
- Standalone: the config fails to load; a SIGHUP reload keeps the running routes.

A backend or mirror name DNS cannot resolve:

- Kubernetes: the backend is left out and its share goes to the other backends; a mirror is dropped.
- The DNS refresh (`dnsRefreshInterval`) brings it back once it resolves.
- Standalone: the config fails to load; a SIGHUP reload keeps the running routes.

## CORS

The `CORS` filter of an HTTPRoute rule, written the same in a config file:

```yaml
filters:
  - type: CORS
    cors:
      allowOrigins: ["https://app.example.com", "https://*.example.org"]
      allowMethods: [GET, PUT]
      allowHeaders: [content-type, authorization]
      exposeHeaders: [x-request-id]
      allowCredentials: true
      maxAge: 600
```

- An origin is allowed when it equals one of `allowOrigins`: scheme, host and port, the scheme's own port counting as written. A host may start with `*`, which stands for anything before the rest, dots included: `https://*.example.org` allows `https://a.b.example.org`, not `https://example.org`. `*` alone allows every origin.
- An allowed origin is echoed in `Access-Control-Allow-Origin`, never answered with `*`. So every response of the rule carries `Vary: Origin`.
- A preflight (`OPTIONS` with `Origin` and `Access-Control-Request-Method`) is answered by portail with a 204 and never reaches a backend, whatever the rule's backends are. From an origin that is not allowed: 403, without CORS headers.
- The preflight answer lists `allowMethods` and `allowHeaders` as configured. Where one holds `*`, what the preflight asks for is echoed instead.
- `maxAge` is 5 seconds when not set.
- On any other request from an allowed origin, the response carries the allow-origin, `allowCredentials` and `exposeHeaders` headers, in place of any the backend sent. From another origin, or without `Origin`, it carries none of them, the backend's included.
- portail's own answers for the rule carry them too: a redirect, a 500, a 502, a 503, a 504. A browser then reports that answer, not a CORS failure.
- `exposeHeaders: ["*"]` is sent as `*` without `allowCredentials`. With it nothing is exposed: browsers refuse the wildcard then.
- One `CORS` filter per rule, none on a backendRef.
- A value that cannot be a header (an origin that is no `<scheme>://<host>[:<port>]`, a method or header name that is no token) rejects the route, as any filter value portail cannot apply.

## Retries

The `retry` stanza of an HTTPRoute rule, written the same in a config file:

```yaml
rules:
  - retry:
      codes: [502, 503]
      attempts: 2
      backoff: 100ms
    backendRefs:
      - name: api
        port: 8080
```

- A request is attempted again when no connection to its backend could be made, when the backend answers one of `codes`, or when it gives no answer within `timeouts.backendRequest`.
- `attempts` counts the tries after the first: one when not written, none at 0, at most 100. The API sets no ceiling; each attempt is a connect a client can ask for, so portail sets one, and a rule above it rejects its route.
- `codes` are 400 to 599. With none, only a failed connect or a backend timeout is tried again.
- Only a request that is whole in portail's hands is tried again: its body arrived with its head, in the connection's read buffer. A request whose body is still arriving, and an upgrade, are never tried again: the first answer is the client's. Nothing is buffered for the sake of a retry.
- Each attempt chooses its backend again among the rule's healthy ones.
- `timeouts.request` bounds all the attempts together, `timeouts.backendRequest` each of them.
- Without `backoff` an attempt follows the failed one at once. With it portail waits before each: the base doubled from one attempt to the next, at most ten times the base, and between half and all of that. A wait the request timeout has no room for is not started: the client gets a 504 at once.
- The client gets the last attempt's answer, whatever it is. A request mirror is copied once, whatever the attempts.
- `portail_http_retries_total` counts the attempts made again.
- The replay of a request on a fresh connection, when a pooled one turns out dead, is apart: it needs no stanza and counts in `portail_pool_stale_retries_total`.

## Backend Services

In Kubernetes mode, HTTP, TCP, UDP and TLS passthrough backends and `RequestMirror` all reach a Service the same way:

- ClusterIP: its DNS name, on the Service port, which kube-proxy maps to the pods.
- Headless: every ready pod DNS returns, on the `targetPort`, as DNS carries no port. A named `targetPort` is read from the Service's EndpointSlices.
- ExternalName: its external name, on the Service port, at one address of the set each pod picks by hash, so pods spread over the set.
- A mirror sends its copies to one address: the smallest the Service resolves to.
- These single addresses change only when the address set does; a reordered DNS answer changes nothing.

## Backend TLS

A backendRef of an HTTP rule, or of its `RequestMirror`, is reached over TLS when its Service port's `appProtocol` is `https`, or when it carries `tls`. Only `tls` verifies the backend:

- `hostname`: the SNI sent, and the name the certificate must carry.
- `subjectAltNames`: when set, the certificate must carry one of them instead, each a `hostname` or a `uri` (a SPIFFE ID), and `hostname` only selects the certificate.
- `ca`: what the certificate must chain to. `system`: the roots of the host portail runs on. `bundle: <name>`: a PEM bundle of the config's `backendCas`.

In a config file:

```yaml
backendCas:
  internal: |
    -----BEGIN CERTIFICATE-----
    ...
    -----END CERTIFICATE-----
httpRoutes:
  - parentRefs: [{name: gateway}]
    hostnames: [api.example.com]
    rules:
      - backendRefs:
          - name: api.internal
            port: 8443
            tls:
              hostname: api.internal
              ca:
                bundle: internal
```

- A backend that fails the check gets no byte of the request. The client gets a 502, and the failure counts against the backend's health as a refused connection does.
- A mirror's copy is sent the way a request to that backend would be, or not at all: never in the clear to an `https` port, never to a backend that fails its check.
- A `tls` that cannot be honoured is never downgraded to an unverified connection: no bundle of that name, a bundle with no certificate, no system root on the host. A config file is refused at load; from a controller the backendRef's share answers 500, and a mirror copies nothing.
- `tls` on a backendRef of a TCP, UDP or TLS route fails that share the same way: only HTTP rules originate TLS. So does `tls` on a port whose `appProtocol` asks for a [PROXY header](deployment.md#proxy-protocol-to-a-service): that connection is made in the clear.
- Without `tls`, an `https` port is encrypted and unverified.
- Idle backend connections are kept per policy: one opened under a policy serves that policy's requests only. A bundle counts by its content, so an edited one is another policy.
- TLS sessions are resumed per policy, across client connections and route changes.

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
| `dnsRefreshInterval` | `5s` | How often backend FQDNs are re-resolved and the route table swapped on change (STRICT_DNS freshness window). In Kubernetes mode a change also re-reads the named `targetPort`s of headless backends; they are re-read every 10 minutes regardless |
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
- An L4 relay or an upgraded tunnel with one direction closed ends after one to two `idleBodyTimeout` without progress.
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

Opt-in via `portail dataplane --http2`, or `--http2` or
`performance.http2: true` standalone. When enabled, TLS-terminate listeners advertise `h2` ahead of
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
  back to HTTP/1.1 for WebSocket automatically. A refused stream gets
  `400` and counts in `portail_http_requests_rejected_total`, as an h1
  CONNECT does.

## Custom Controller Name

To use a custom controller name (e.g., for running multiple Portail instances):

```bash
portail controller --controller-name my-org.example.com/gateway-controller
```

Add the flag to the controller Deployment's args, and update the `GatewayClass` to match:

```yaml
apiVersion: gateway.networking.k8s.io/v1
kind: GatewayClass
metadata:
  name: portail
spec:
  controllerName: my-org.example.com/gateway-controller
```

A second install in one cluster also needs its own namespace, and its own names for the ClusterRole, the ClusterRoleBinding and the GatewayClass.

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

The controller reads TLS certificates from the Kubernetes Secrets referenced in a Gateway listener's `certificateRefs`, and sends them to that Gateway's data planes only. It works with cert-manager or any other Secret-based certificate management.

A changed Secret reaches the data planes in their next sync, without a restart.

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

### Shared TLS Ports

HTTPS and TLS passthrough listeners on one port share one socket:

- The ClientHello's SNI selects one listener, by Gateway API precedence across HTTPS and TLS alike: exact hostname, then the longest wildcard, then the listener without a hostname.
- A wildcard covers any depth: `*.example.com` takes `a.b.example.com`.
- An HTTPS listener terminates with its own certificate and routes HTTP by `Host`; a TLS passthrough listener forwards the raw stream by its TLS routes.
- No SNI, or an SNI no listener names: the listener without a hostname takes it; with none, the connection closes, counted in `portail_listener_tls_handshake_failures_total`.
- A TLS route matches within its listener only: its hostnames outside the listener's are ignored, and a route without hostnames takes the listener's.
- A TLS route without parentRefs (a config built without validation) serves every passthrough listener, ranked with that listener's own routes: the exact hostname, then the longest wildcard, then the route without hostnames; on a tie the listener's own.
- Connections rotate over a TLS route's backends per route, whatever SNI each sent under it.
- Each Gateway change re-plans the port, and the next connection follows it, without a rebind: a protocol change, a listener added or removed, a certificate rotated or deleted.
- A port whose HTTPS certificates fail to load closes the connections its HTTPS listeners select, and is not bound when no passthrough listener shares it.
- A port already serving whose certificates all fail on a later pass, with the same listeners and Secrets, keeps serving the last good ones: a bad rotation does not take HTTPS down. The failure is logged each pass until fixed. A pass that also changes the port's listeners or Secrets keeps none.
- In Kubernetes mode only an invalid rotation of the same Secret keeps the last good certificate, and the listener reports `ResolvedRefs=False` meanwhile.
- That last good certificate lives in the controller's memory: a controller restarted during a bad rotation refuses the listener's hostnames until the Secret is fixed.
- A deleted Secret, or one a ReferenceGrant no longer permits, refuses its hostnames from the controller's next pass.
- An HTTPS listener whose own certificates fail to load closes the connections its SNI selects, even while its siblings' load: no other listener's certificate answers for its hostname.
