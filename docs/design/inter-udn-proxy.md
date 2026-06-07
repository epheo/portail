# Design note: proxying between OVN-Kubernetes User Defined Networks (UDNs)

Status: research/design — records verified platform behaviour and a recommended architecture.
No code change is implied by this document alone.
Scope: how portail should model a Gateway that routes L4/L7 traffic **between** OVN-Kubernetes
UDNs (the `spec.addresses: [{type: portail.epheo.eu/Network}]` "multi-network" mode).
Context: OpenShift 4.18–4.20 / OVN-Kubernetes, verified 2026-06 (single-node OCP 4.21 demo).

## TL;DR

- **One proxy pod cannot bridge two tenants' Primary UDNs** — three independent platform guards
  forbid attaching a Primary UDN as a secondary interface (all reproduced live; §3).
- **Bridge over a `role:Secondary` transit network instead** — a `ClusterUserDefinedNetwork`
  (CUDN) of role Secondary, which the proxy and the participating backends both attach (§5).
- **Put the client-facing front door on a *primary* UDN, fronted by a Service** — Services work
  on primary UDNs (verified), giving the **stable VIP + load-balancing** secondary UDNs lack, and
  putting that listener back in **service-mode** (unprivileged bind, no `NET_BIND_SERVICE`). Use
  secondary UDNs for the *backend-reach* side (§5, Option A — recommended).
- **The current multi-network data plane does not actually work**: it binds the published port
  (80) directly on the UDN IP and fails `EACCES` (§2). Fix = a **file capability** on the binary
  (`setcap cap_net_bind_service`); combined with the operator's existing `add:[NET_BIND_SERVICE]`
  it binds `:80` **under plain `restricted-v2`** — non-root, no root, no privesc, no sysctl, no
  custom SCC. **Tested end-to-end on CRI-O** (§3.5).
- Portail's role is **L4/L7 mediation**, complementary to (not a replacement for) the platform's
  L3 connectivity — secondary UDNs today, `ClusterNetworkConnect` (OKEP-5224) when GA (§6).

## 1. Problem

A portail Gateway can carry `spec.addresses` of type `portail.epheo.eu/Network`, naming UDNs the
data plane should sit on and route between (e.g. `udn-backend` ↔ `udn-frontend`). Today the
operator attaches the per-Gateway pod to each named UDN as a **Multus secondary** network
(`internal/controller/resources.go:335`), keeps eth0 on the default network for kube-apiserver
access, and the data plane binds the **published port directly** on each UDN interface IP. We need
to know whether this is correct, whether it works, and how it should be modelled — including
whether a single proxy can span tenants' isolated **Primary** UDNs.

## 2. Does the current implementation work? No.

Observed on the demo `test` Gateway (addresses `udn-backend`/`udn-frontend`, secondary L3 UDNs):
the data plane loops on

```
Ensuring data plane listeners for ports: [80]
Failed to bind port 80: bind worker 0: Permission denied (os error 13)
```

on **both** the `:0.1.11` pods *and* the older `:latest` pods. No pod has a listening socket on
port 80 of the UDN IPs, so inter-UDN traffic on `:80` is refused. The old `:latest` pods report
`Ready=True` only because their older readiness probe didn't gate on port binding — a false
positive; v0.1.11 correctly reports not-ready.

Root cause: in multi-network mode there is **no fronting Service** to remap the published
(privileged) port to an unprivileged `target_port` (`src/kubernetes/reconciler.rs:185`), so the
data plane binds `:80` directly (`src/kubernetes/controller.rs`, `ensure_data_plane_listeners` /
`compute_bind_addresses`). The operator adds `NET_BIND_SERVICE` to the container caps for this case
(`resources.go:258`) — but that capability is a **no-op for the non-root, `no_new_privs` pod**
(`runAsNonRoot: true`, `allowPrivilegeEscalation: false`, no ambient caps, no file cap). Hence
EACCES. The intent is right; the mechanism cannot work under restricted Pod Security.

## 3. Verified platform facts (OCP 4.18–4.20)

### 3.1 UDN model & multi-attachment
- A namespace has **at most one Primary UDN**, **auto-attached to every pod** in it; a pod
  **cannot hold more than one Primary** (besides the cluster-default), and the cluster-default +
  primary attach in a single CNI ADD.
- A pod **can** attach **multiple Secondary UDNs** via Multus; both Layer2 and Layer3 support
  **both** `Primary` and `Secondary` roles. Layer3 secondary nets are routed, east-west only, no
  egress. Keeping eth0 on the default network for API/egress is correct. [OKEP-5193; multi-homing]

### 3.2 A Primary UDN CANNOT be attached as a secondary interface (proved live)
The `role` is fixed at network-definition time and determines the attachment mechanism — primary =
auto, same-namespace only; secondary = the only role selectable via `k8s.v1.cni.cncf.io/networks`.
Three independent guards, each reproduced on the cluster:

| Attempt | Outcome |
|---|---|
| Pod in `default` references `tenant-a/network-a` | **Multus namespace isolation**: `namespace isolation enabled, annotation violates permission, pod is in namespace default but refers to target namespace tenant-a` (cross-ns refs limited to the `globalNamespaces` allowlist). |
| Pod in `tenant-a` lists its own `network-a` (role primary) | **OVN-K role guard**: `unexpected primary network "tenant-a_network-a" specified with a NetworkSelectionElement`. |
| A hand-written `role:Secondary` NAD aliasing the same OVN network (`name: tenant-a_network-a`) | Degenerate/non-functional: `network-a` is already the auto primary; the extra secondary attach never completes — pod wedged in `ContainerCreating`, no second interface, empty network-status. |

Conclusion: **no supported way to bring a Primary UDN onto another pod as a secondary interface.**
This is by design (it *is* the isolation guarantee). Disabling namespace isolation only clears the
cross-ns guard, not the role guard.

### 3.3 Cross-UDN isolation & bridging
- Distinct UDNs are **isolated by design**; east-west between two different UDNs is blocked and
  **NetworkPolicy cannot open it**. The only **pod-level** bridge today is a **dual-homed pod on a
  shared network both sides can join** (i.e. a shared *secondary* UDN). [ovn-k UDN; multi-homing]
- The sanctioned admin-level bridge for **Primary** networks is **`ClusterNetworkConnect`
  (OKEP-5224)** — **Primary-only and not GA**. Design toward it; do not depend on it yet.

### 3.4 Services: not on secondary UDNs, YES on primary UDNs → VIP implication
- **Kubernetes Services do not work on secondary UDNs** → a multi-replica proxy exposed on a
  secondary UDN has **no stable VIP** (the Gateway address resolves to a single pod IP via
  `resolve_network_to_local_ip`). VIP tooling (MetalLB-L2, kube-vip, keepalived) is single-holder
  **failover**, not distributed LB, and is **unvalidated on secondary UDNs**.
- **Services DO work on primary UDNs** (verified): in `tenant-a`, Service `server-a`
  (`172.30.119.27:8080`) has an OVN-mirrored EndpointSlice with endpoints on the **primary-UDN
  subnet** (`10.150.0.4/5`) — i.e. a real ClusterIP VIP load-balanced across replicas *on the
  primary network*. This is the lever for solving the VIP problem (§5, Option A).

### 3.5 Privileged-port binding under restricted Pod Security — **TESTED on CRI-O (OCP 4.21)**
- **A file capability on the binary is the fix, and it stays fully `restricted-v2`** — no custom
  SCC, no root, no `allowPrivilegeEscalation`, no sysctl. Verified end-to-end: a capped binary
  (`cap_net_bind_service=ep`) in a pod that is `runAsNonRoot`, `allowPrivilegeEscalation: false`,
  `readOnlyRootFilesystem`, seccomp `RuntimeDefault`, `drop:[ALL]` **`add:[NET_BIND_SERVICE]`**
  **bound `:80`** and was admitted under **`restricted-v2`** (which already lists
  `allowedCapabilities: [NET_BIND_SERVICE]`).
- **Why it works (correcting earlier assumptions):** `NET_BIND_SERVICE` in `capabilities.add` is a
  no-op for non-root *only when the binary has no file cap* — it puts the cap in the **bounding**
  set but not the effective set after exec. Add the **file cap** and the two compose: the file cap
  raises the cap into the effective set, bounded by the cap-add. `no_new_privs` (from
  `allowPrivilegeEscalation: false`) does **not** block this — the cap is already in the
  bounding/permitted set, so it isn't a privilege *escalation*. (The earlier "file caps defeated by
  no_new_privs" and "drop the no-op cap" notes were wrong for this configuration — the cap-add is
  *required*, not dropped.) The `net.ipv4.ip_unprivileged_port_start` sysctl also works but is
  **rejected** (semantic hack — see Appendix).
- **One constraint — the effective bit breaks service-mode (TESTED):** a `+ep` (effective-bit) file
  cap makes the binary **fail to exec** when `NET_BIND_SERVICE` is *not* in the bounding set —
  exactly the service-mode pod (`drop:[ALL]`, no cap-add): `exec … Operation not permitted`. Since
  one image serves both modes, resolve with one of:
  - **`+p` (permitted-only) + portail raises the cap itself** *(preferred — truly minimal)*:
    `setcap cap_net_bind_service=+p`. `+p` has no effective bit so **exec never fails**; service-mode
    pods run with **zero capabilities** (cap not in bounding → not in permitted → portail skips the
    raise and binds its unprivileged target port). Privileged-bind pods get the cap in *permitted*
    (cap-add puts it in bounding), and the data plane **raises it to effective** (`prctl`/`caps`
    crate) before binding `:80`. Small portail code change.
  - **`+ep` + operator adds `NET_BIND_SERVICE` in *all* modes** *(simpler — no code change)*: the
    cap is always in the bounding set so exec never fails; service-mode pods carry an **unused**
    `NET_BIND_SERVICE`. Slightly less minimal for service-mode.
- **Net:** service-mode Gateways bind an unprivileged target port (Service remaps); privileged-bind
  Gateways (multi-network/UDN, host-network daemonset) bind the **real** published port via the
  file cap — all under `restricted-v2`, non-root, no privesc, no root, no sysctl, no custom SCC.

## 4. Not viable

- **One proxy straddling two tenants' Primary UDNs** — impossible (§3.2). Do not pursue.
- **All listeners exposed on secondary UDNs** — works for reachability but gives no VIP and forces
  the privileged-bind workaround on the client side; only acceptable for single-instance / low-HA
  client sides.

## 5. Design options

### Option A — Primary front-door + secondary backend-reach (recommended)
Run the Gateway pod in a namespace whose **primary UDN** is the client-facing network; expose the
client listener via a **Service on that primary** (stable VIP + LB, and **service-mode** so the
pod binds an unprivileged target port — no `NET_BIND_SERVICE`). Attach **secondary** UDNs to the
same pod for **backend reach**; the proxy is the client there, so no VIP is needed (bind a high
port on the secondary IP).
- Wins: solves the VIP/HA gap and the privileged-bind problem in one move (both on the primary
  side).
- Constraints: **hub topology** — a primary VIP only serves clients on *that same primary*
  (cross-primary isolation), so clients + gateway share one primary; multiple client-primaries ⇒ a
  Gateway per client-primary. **Backends must live on the attached secondary UDNs** (no cross-
  primary reach), and since Services/EndpointSlices don't exist there, backend discovery is
  **static/headless** (portail STRICT_DNS-style fixed addressing), not Service-based.

### Option B — Shared `role:Secondary` CUDN transit (single proxy)
Define a `ClusterUserDefinedNetwork` of **role Secondary** spanning the relevant namespaces (a CUDN
materialises its NAD in each selected namespace, so each pod references it **same-namespace** —
sidestepping Multus isolation — and it's Secondary — sidestepping the role guard). The proxy and
the participating backends both attach it. Tenants keep Primary isolation; the CUDN is the
deliberate cross-tenant L7 path. Cost: intrusive (each participating workload must attach the
transit net); high-port bind applies.

### Option C — Proxy-pair stitched by a shared Secondary CUDN
One Gateway per tenant namespace (each gets that tenant's Primary auto-attached, reaching its own
backends natively) **plus** the shared Secondary CUDN to reach the peer proxy:
`client@A → proxy-A (primary-A) → shared Secondary → proxy-B → backend@B (primary-B)`. Workloads
stay untouched; only the proxies are dual-homed. Cost: needs a peer-Gateway/transit concept portail
does not yet have.

### Future — `ClusterNetworkConnect` (OKEP-5224)
When GA, it connects Primary UDNs at L3 admin-side; portail then rides on connected primaries
instead of hand-rolled secondary transit, while still providing the L7 mediation. Track it.

## 6. Positioning: complementary to OKEP-5224 (portail should not own L3)

- **OKEP-5224 = L3 connectivity** between networks (the plumbing; *broadens* reachability).
- **Portail = L4/L7 gateway** (host/path routing, TLS, policy, observability; *mediates*
  reachability as a default-deny chokepoint that preserves isolation).

They compose. Today portail supplies both the transit (a secondary UDN) and the L7 gateway on it;
post-5224 the platform supplies primary-to-primary connectivity and portail supplies the L7 control
point. Design principle: **portail consumes whatever shared transit exists and stays in the L7
lane** rather than trying to own network connectivity.

## 7. Implications for portail

**Port-binding piece: IMPLEMENTED and validated on CRI-O (2026-06-05).** `Containerfile` bakes
`setcap cap_net_bind_service=+p`; `src/privileges.rs::raise_net_bind_service()` raises it to
effective in `src/main.rs` before the runtime starts; the operator's `add:[NET_BIND_SERVICE]` for
network mode is kept (load-bearing); the host-network/multi-network examples drop the sysctl for
`add:[NET_BIND_SERVICE]`. Verified with the real binary under `restricted-v2`: service-mode pod
(no cap) execs + binds 8080; privileged pod (cap-add) raises the cap and binds `:80` (`curl` → 404).
The VIP / network-topology pieces below remain design-only.

- **Image:** add `setcap cap_net_bind_service=+p` on the portail binary in the `Containerfile`
  (binary at `/usr/local/bin/portail`). Use **`+p`** (permitted-only) so the binary still execs in
  service-mode pods that drop the cap (`+ep` would `EPERM` on exec there — §3.5).
- **Data plane:** always bind the **real published port** (`controller.rs`
  `compute_bind_addresses`/`required_endpoints`/`ensure_data_plane_listeners`; `reconciler.rs:185`).
  In privileged-bind modes, **raise `cap_net_bind_service` to effective** (`caps` crate) before
  binding; in service-mode the cap isn't present so skip the raise and bind the unprivileged
  `target_port`. No high-port hack, no sysctl.
- **Operator:** **keep** the `add:[NET_BIND_SERVICE]` in network mode (`resources.go:258`) — with
  the file cap it is now *load-bearing* (puts the cap in the bounding set), not a no-op. **No SCC
  change** — the pod stays `restricted-v2`. Service-mode Gateways keep `restricted-v2` with no
  cap-add. For Option A's front door, deploy the Gateway pod with its namespace primary + secondary
  attachments and create the **Service on the primary** (reusing the service-mode targetPort pool,
  `resources.go:91-130`).
- **API/model:** to support "listeners on different secondary UDNs", add a **per-listener network
  association** (today `spec.addresses` is per-Gateway and every listener binds every address; the
  data plane already binds per resolved IP, so this is a model extension, not a rewrite).
- **Backend discovery on secondary UDNs:** static/headless (no Services there).

## 8. Caveats / to verify
- `ClusterNetworkConnect` (OKEP-5224) and ambient caps (KEP-2763) are **not GA**.
- MetalLB / kube-vip **on a secondary UDN** is **unvalidated**.
- `net.ipv4.ip_unprivileged_port_start` was **confirmed working** (safe/`restricted-v2`-compatible)
  on OCP 4.21, but is **rejected on design grounds** (§3.5) — recorded as a tested-but-not-chosen
  alternative, not a recommendation.
- Option C (peer-Gateway transit) and per-listener network binding are **not implemented** in
  portail today.
- **The file-cap `:80` bind is VALIDATED on CRI-O** (OCP 4.21) — see §3.5 and the Appendix. Two
  things to keep in mind when porting the recipe: use **`+p`** not `+ep` (else service-mode pods
  `EPERM` on exec — tested), and don't validate this with local `podman` — podman grants `--cap-add`
  via the **ambient set** for non-root (an *uncapped* binary bound `:80` with just `--cap-add`),
  which CRI-O does **not** do, so podman can't isolate the file-cap behaviour. Re-confirm the `+p`
  + self-raise path once portail implements the cap raise.

## Appendix — reproductions (OCP 4.21 demo, 2026-06)
- Cross-ns primary-as-secondary → Multus `annotation violates permission … target namespace tenant-a`.
- Same-ns primary-as-secondary → OVN-K `unexpected primary network … specified with a NetworkSelectionElement`.
- Secondary-NAD-aliasing-primary → pod stuck `ContainerCreating`, only `eth0 [default, primary]`, no 2nd iface.
- Services on primary UDN → `server-a` ClusterIP, mirrored EndpointSlice endpoints `10.150.0.4/5` (primary subnet).
- `test` data plane → recurring `Failed to bind port 80: Permission denied (os error 13)` on `:latest` and `:0.1.11`.
- sysctl bind → a pod with the full restricted profile + `net.ipv4.ip_unprivileged_port_start=80` was admitted and logged `Serving HTTP on 0.0.0.0 port 80`; `restricted-v2.forbiddenSysctls` is empty. (Works, but rejected — §3.5.)
- **file cap, CRI-O (the chosen path):** built an image with `setcap cap_net_bind_service=+ep` (`getcap` → `…/python3.9 cap_net_bind_service=ep`); a non-root pod with `allowPrivilegeEscalation:false`, `drop:[ALL] add:[NET_BIND_SERVICE]`, read-only rootfs, seccomp **bound `:80` (`BIND_OK_80`) under `scc=restricted-v2`** — no custom SCC, no privesc, no root.
- **`+ep` service-mode break:** the same capped image with `drop:[ALL]` and **no** cap-add failed to start: `exec container process /usr/bin/python3: Operation not permitted` → use `+p`.
- **podman ≠ CRI-O:** an *uncapped* python with `--cap-add NET_BIND_SERVICE` (non-root) bound `:80` under podman (ambient caps) but the same `capabilities.add` is a no-op on CRI-O — so local podman cannot validate this.

## References
- OVN-K UDN: https://ovn-kubernetes.io/features/user-defined-networks/user-defined-networks/
- OVN-K multi-homing: https://ovn-kubernetes.io/features/multiple-networks/multi-homing/
- OKEP-5193 (UDN): https://ovn-kubernetes.io/okeps/okep-5193-user-defined-networks/
- OKEP-5224 (Connecting UDNs): https://ovn-kubernetes.io/okeps/okep-5224-connecting-udns/okep-5224-connecting-udns/
- OVN-K UDN API spec: https://github.com/ovn-kubernetes/ovn-kubernetes/blob/master/docs/api-reference/userdefinednetwork-api-spec.md
- RH primary networks: https://docs.redhat.com/en/documentation/openshift_container_platform/4.20/html/multiple_networks/primary-networks
- MetalLB L2: https://metallb.universe.tf/concepts/layer2/
- kube-vip ARP: https://kube-vip.io/docs/modes/arp/
- KEP-2763 ambient caps: https://github.com/kubernetes/enhancements/blob/master/keps/sig-security/2763-ambient-capabilities/README.md
- restricted SCC + NET_BIND_SERVICE: https://github.com/redhat-openshift-ecosystem/community-operators-prod/discussions/1417
