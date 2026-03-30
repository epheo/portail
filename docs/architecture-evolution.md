# Portail Architecture Evolution — Working Document

## Current State

Portail is a single Rust binary that is both control plane and data plane. Deployed as a DaemonSet with `hostNetwork: true`, each pod binds directly to node interfaces on `0.0.0.0:<port>`. Each node reports its own NODE_IP as the Gateway address. The control plane and data plane share memory via `ArcSwap<RouteTable>` — zero IPC, zero serialization.

Key existing capabilities:
- `ListenerConfig` already has `address: Option<String>` and `interface: Option<String>` fields
- SO_BINDTODEVICE support in `create_reuseport_tcp_listener()`
- Dynamic port binding, TLS hot-reload
- Multi-gateway support with per-gateway config caching and merged route tables
- TCPRoute, UDPRoute, TLSRoute, HTTPRoute — full L4-L7
- 100% conformance on GATEWAY-HTTP and GATEWAY-TLS profiles

## Problems With Current Model

1. **Not multi-node HA** — each node's gateway is independent, no shared VIP, if the node dies the gateway IP is gone
2. **Not Kubernetes-idiomatic** — standard pattern is Deployment + Service(type=LoadBalancer) per Gateway
3. **Can't isolate Gateways** — all Gateways share the same DaemonSet, same network namespace, same ports
4. **Multi-interface** — multiple interfaces (eth0/eth1) with different routes per interface is awkward in a shared DaemonSet

## Core Insight: One Gateway = One Address = One Pod = One Network Context

Every major Gateway API implementation (Envoy Gateway, Istio, Kong) follows the pattern: each Gateway CR gets its own Deployment and its own Service(type=LoadBalancer). Each Gateway is an isolated unit.

Portail should follow this pattern, but without building a complex provisioner/operator. The binary stays simple — the deployment topology is the user's (or platform's) concern.

### The "Unmanaged Infrastructure" Model

The Gateway API spec (GEP-1867) explicitly recognizes two models:
- **Self-managed**: implementation creates pods/services when a Gateway CR appears (Envoy Gateway, Istio)
- **Unmanaged**: data plane pre-exists, Gateway CRs configure it (Cilium, NGINX, Portail)

Portail stays unmanaged. The user (or their platform tooling, GitOps, etc.) creates the Deployment. Portail just runs in it and serves the Gateway.

## Minimal Code Changes Required

### 1. Bind listeners to `spec.addresses[0].value` instead of `0.0.0.0`

Currently `convert_listener()` in `converters.rs` never sets `ListenerConfig.address` — it's always None, meaning `0.0.0.0`. If it reads from the Gateway's `spec.addresses`, listeners bind to the specific IP.

This is the key change. It enables:
- **Multi-network**: same port (e.g., 80) bound on different IPs for different UDNs
- **Natural isolation**: Portail reconciles all Gateways, but only successfully binds to addresses that exist on the pod's interfaces. Gateways with unreachable addresses get `Programmed=False`. The network is the filter — no `--gateway-ref` flag needed.
- **Backwards compatible**: no `spec.addresses` = bind to `0.0.0.0` (current behavior)

### Status reporting: no change needed

The existing `spec.addresses` mechanism (`GatewayStaticAddresses` in `status.rs`) already handles all deployment patterns:
- **Deployment + LB**: user sets `spec.addresses` to the LB VIP, Portail reports it in status
- **Multi-network**: user sets `spec.addresses` to the UDN IP, Portail reports it in status
- **DaemonSet (no spec.addresses)**: falls back to `NODE_IP → POD_IP → 0.0.0.0` as today

### What Does NOT Change

- Data plane code (data_plane.rs, worker.rs, routing.rs, http_filters.rs)
- Route reconciliation logic (reconciler.rs)
- Config types (config/types.rs)
- Status reporting logic (status.rs)

## Deployment Model

The user creates their own manifests. Portail provides examples.

### Single-address Gateway (typical)

```yaml
apiVersion: apps/v1
kind: Deployment
metadata:
  name: portail-public
spec:
  replicas: 2
  template:
    spec:
      containers:
      - name: portail
        image: ghcr.io/epheo/portail:latest
        args: ["--kubernetes"]
        ports:
        - containerPort: 80
        - containerPort: 443
---
apiVersion: v1
kind: Service
metadata:
  name: portail-public
  annotations:
    metallb.universe.tf/address-pool: "public-pool"
spec:
  type: LoadBalancer
  selector:
    app: portail-public
  ports:
  - name: http
    port: 80
  - name: https
    port: 443
---
apiVersion: gateway.networking.k8s.io/v1
kind: Gateway
metadata:
  name: public-gw
spec:
  gatewayClassName: portail
  addresses:
    - type: IPAddress
      value: "203.0.113.10"    # MetalLB VIP
  listeners:
    - name: http
      port: 80
      protocol: HTTP
    - name: https
      port: 443
      protocol: HTTPS
      tls:
        certificateRefs:
          - name: my-cert
```

No hostNetwork, no NET_BIND_SERVICE, no SCC. The Service handles port exposure. MetalLB (or any LB provider) assigns the VIP. Portail binds to container ports and the Service maps them.

### Multi-interface (eth0 public, eth1 private)

Two Gateways, two Deployments, two Services with different MetalLB pools. No Portail code knows about interfaces — it's all LB configuration.

### DaemonSet mode (unchanged, for single-node/edge)

Existing `examples/kubernetes/daemonset/` continues to work. All Gateways reconciled by every pod. hostNetwork for direct port binding.

## The VPC / Multi-Tenant Routing Use Case

### Context

In a Kubernetes-native IaaS platform (e.g., KubeVirt + UDN), tenants create their own networks (UDNs), VMs, and pods. They need self-service routers between their network segments — both north-south (ingress/egress) and east-west (inter-subnet).

### Portail as Tenant Router

A Portail Gateway, deployed as a pod attached to multiple UDNs via Multus/UDN network attachments, becomes the tenant's router between their subnets.

```
Tenant "acme" owns:
  UDN-frontend (10.10.1.0/24)
  UDN-backend  (10.10.2.0/24)
  UDN-database (10.10.3.0/24)

┌─────────────┐    ┌─────────────┐    ┌─────────────┐
│ UDN-frontend│    │ UDN-backend │    │ UDN-database│
│ 10.10.1.0/24│    │ 10.10.2.0/24│    │ 10.10.3.0/24│
│             │    │             │    │             │
│ web VMs     │    │ app VMs     │    │ db VMs      │
└──────┬──────┘    └──────┬──────┘    └──────┬──────┘
       │                  │                  │
       └──────────┬───────┴──────────────────┘
                  │
        ┌─────────┴──────────┐
        │  Portail pod       │
        │  10.10.1.1 (fe)    │
        │  10.10.2.1 (be)    │
        │  10.10.3.1 (db)    │
        │                    │
        │  reconciles all    │
        │  acme/ Gateways    │
        └────────────────────┘
```

### How It Works

Each UDN = a separate Gateway (one address, one network context):

```yaml
apiVersion: gateway.networking.k8s.io/v1
kind: Gateway
metadata:
  name: frontend-gw
  namespace: acme
spec:
  gatewayClassName: portail
  addresses:
    - type: IPAddress
      value: "10.10.1.1"
  listeners:
    - name: http
      port: 80
      protocol: HTTP
    - name: tcp-all
      port: 443
      protocol: TCP
```

Routes express inter-subnet connectivity:

```yaml
# Frontend can reach backend app servers
apiVersion: gateway.networking.k8s.io/v1
kind: HTTPRoute
metadata:
  name: frontend-to-backend
  namespace: acme
spec:
  parentRefs:
    - name: frontend-gw
  hostnames: ["api.internal"]
  rules:
    - backendRefs:
        - name: app-service    # on UDN-backend
          port: 8080

---
# Frontend can SSH to backend (L4, no HTTP inspection)
apiVersion: gateway.networking.k8s.io/v1alpha2
kind: TCPRoute
metadata:
  name: frontend-ssh-to-backend
  namespace: acme
spec:
  parentRefs:
    - name: frontend-gw
      sectionName: tcp-all
  rules:
    - backendRefs:
        - name: app-vm-ssh    # on UDN-backend
          port: 22
```

### Why This Works

- **NAT is implicit.** Portail accepts on one UDN interface, opens a new connection on another. The destination sees Portail's UDN IP as source. Application-level NAT with no iptables, no conntrack, no kernel privileges.
- **Network attachment IS the security boundary.** The pod is only attached to the tenant's UDNs. It physically cannot route to other tenants' networks. Stronger than RBAC or network policy.
- **Self-service via standard Kubernetes API.** Tenant creates Gateway + Routes in their namespace. No cluster-admin needed. The platform just ensures the Deployment has the right network attachments.

### Portail vs L3 Infrastructure: Complementary, Not Competing

Pure L3 subnet routing (forward any packet between subnets) belongs to the infrastructure layer — OVN routers, iptables masquerade, or kernel `ip_forward`. These operate in kernel space with no per-connection overhead, no context switches, no userspace copies. They will always outperform a userspace proxy for raw L3/L4 forwarding.

Portail is a userspace proxy. Every connection involves: TCP accept → connection state → new backend connection → bidirectional byte copy via syscalls. This overhead is negligible for application traffic but meaningful at scale for bulk L3 forwarding.

**The right architecture is layered:**
- **OVN / iptables** handles baseline L3 connectivity between tenant UDNs (fast, transparent, kernel-level)
- **Portail** handles the L4-L7 policy layer on top (powerful, explicit, self-service)

Tenants get basic inter-subnet connectivity from the infrastructure. When they need finer control — HTTP path routing, TLS termination, load balancing, traffic mirroring, header-based decisions — they point specific traffic through their Portail Gateway.

Portail's value is when the tenant wants **more than L3**:
- "Route HTTP traffic from frontend to backend with path-based rules"
- "Terminate TLS at the subnet boundary and re-encrypt toward backends"
- "Mirror 5% of traffic to a canary on the other subnet"
- "Load balance across 3 VMs with weighted round-robin"
- "SSH to specific VMs via port-mapped TCPRoutes through the gateway"

For everything else, the L3 infrastructure handles it transparently.

### What the Platform Controller Does (Not Portail)

A higher-level platform controller (separate project) would automate:
1. Tenant requests "router between UDN-A and UDN-B"
2. Controller creates a Deployment with Multus annotations for both UDNs
3. Controller creates Gateway CRs with the right addresses
4. Portail binary in the pod does the rest

Portail doesn't need to understand any of this. It just sees a Gateway with an address and routes.

## Extensibility: Beyond Standard Gateway API

Gateway API covers the core routing needs, but the VPC use case will eventually require features the spec doesn't define yet. Two spec-compliant extension mechanisms:

### 1. Custom BackendRef Kinds — IP Backends

Gateway API BackendRefs support arbitrary `group` and `kind` fields. GEP-1742 proposes `kind: IPAddress` for routing to raw IPs. Portail should implement this early.

```yaml
apiVersion: gateway.networking.k8s.io/v1alpha2
kind: TCPRoute
spec:
  parentRefs:
    - name: frontend-gw
  rules:
    - backendRefs:
        - kind: IPAddress
          name: "10.10.2.50"
          port: 22
```

On the Portail side, backend resolution already produces `(ip, port)` tuples via `endpoint_overrides`. An IPAddress backend skips the Service lookup and goes straight to `(name_as_ip, port)`. The reconciler (`reconciler.rs`) currently validates `group: ""` and `kind: "Service"` and rejects everything else — this just needs to accept `kind: "IPAddress"` as well.

This covers: VMs on UDNs without Kubernetes Services, external hosts, bare-metal endpoints.

### 2. Policy Attachments

For features beyond the Gateway API spec, custom CRDs via the Policy Attachment pattern (GEP-2648 / GEP-713). These attach to existing Gateway API resources via `targetRef` — the same mechanism Cilium, Istio, and Envoy Gateway use for their extensions.

Future candidates as needs emerge: source NAT control, bandwidth/QoS policies, cross-tenant peering rules.

### Extension Strategy

- **Start with annotations** for experimental features (zero CRD overhead)
- **Graduate to Policy Attachments / custom BackendRef kinds** once a feature stabilizes
- Only add CRDs when the feature genuinely needs schema validation and discoverability

### Relation to Gateway API Spec Evolution

- **GEP-1742 (IP backends)**: Portail implementing `kind: IPAddress` early means compliance when it merges
- **GEP-1867 (infrastructure model)**: Validates Portail's unmanaged approach
- **GAMMA (mesh profile)**: Not relevant — GAMMA is single-network transparent interception, Portail is explicit multi-network routing. Different problem.
- **NAT/SNAT/DNAT**: Not in any GEP. Gateway API considers NAT an infrastructure concern. Portail's userspace proxying provides implicit application-level NAT (backend sees Portail's IP as source). Pure L3 NAT/routing belongs to the infrastructure layer (OVN, iptables) — Portail complements this as the L4-L7 policy layer, not replaces it.

## Implementation Progression

Each step is small, self-contained, and independently useful:

### Step 1: Bind listeners to `spec.addresses`
- Bind listeners to `spec.addresses` instead of `0.0.0.0` when set
- Gateways with unreachable addresses get `Programmed=False` (natural isolation)
- Status reporting already works via `spec.addresses` (no change needed)
- Enables Deployment-per-Gateway model and multi-network listener isolation
- Small change in `converters.rs`

### Step 2: `kind: IPAddress` in backend refs
- Enables routing to VMs, external hosts, raw IPs
- Skip Service lookup when kind is IPAddress, use name as IP directly
- Small change in reconciler validation + backend resolution

### Future steps (as needs emerge, via Policy Attachments)
- Source NAT control
- Bandwidth/QoS policies
- Cross-tenant peering (controlled routing between tenant UDNs)

## Open Questions

- **Port overlap between Gateways.** Two Gateways both want port 80 but on different addresses (10.10.1.1:80 vs 10.10.2.1:80). This works when listeners bind to `spec.addresses` instead of 0.0.0.0. But today Portail detects port conflicts at the Gateway level — that logic needs to become address-aware.

- **Service discovery across UDNs.** When a TCPRoute references `app-service` on UDN-backend, how does Portail resolve that? Standard kube-dns resolves to ClusterIPs, which may not be reachable from a UDN. Headless services + EndpointSlice with pod IPs on the UDN might work. Needs investigation.

- **Scale.** In a multi-tenant platform, there could be hundreds of tenant routers. Is one Deployment per router too heavy? Could a shared Portail instance serve multiple tenants with namespace-scoped Gateways? (Tension with network isolation.)

- **Future: managed mode.** If demand grows, a thin provisioner (`--provisioner` flag, same binary) could automate Deployment+Service creation. But this is optional and separate from the core changes.
