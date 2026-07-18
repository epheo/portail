# Deployment

## Prerequisites

- Kubernetes 1.28+ (tested on MicroShift, should work on any conformant cluster)
- Gateway API CRDs installed (experimental channel recommended)

```bash
kubectl apply -f https://github.com/kubernetes-sigs/gateway-api/releases/download/v1.4.1/experimental-install.yaml
```

## DaemonSet with Host Networking

One pod per node with `hostNetwork: true`. Simplest setup, good for single-node and edge.

```bash
kubectl apply -k examples/kubernetes/daemonset/
```

The pod binds directly to the node's network interfaces. Ports defined in your Gateway listeners (e.g., 80, 443) are opened on the node. No Kubernetes Service is needed. Privileged ports (< 1024) are bound via `cap_net_bind_service`, carried as a **file capability** on the portail binary (`setcap …=+p` in the image) and added to the pod (`capabilities.add: [NET_BIND_SERVICE]`); portail raises it to effective at startup. The pod stays **non-root** with `allowPrivilegeEscalation: false` and **no sysctl** — fully restricted Pod Security.

If your node has a firewall, open the ports your Gateway listeners use:

```bash
sudo firewall-cmd --add-port=80/tcp --add-port=443/tcp --permanent && sudo firewall-cmd --reload
```

On OpenShift / MicroShift, apply the SecurityContextConstraints for `hostNetwork` and `NET_BIND_SERVICE`:

```bash
kubectl apply -f examples/kubernetes/daemonset/scc.yaml
```

See: `examples/kubernetes/daemonset/`

## Deployment with LoadBalancer Service

One Deployment per Gateway, exposed via a LoadBalancer Service. Requires MetalLB or a cloud LB provider. Best for multi-node production.

```bash
kubectl apply -k examples/kubernetes/loadbalancer/
```

Portail runs as a regular Deployment (no `hostNetwork`). A Kubernetes Service exposes it externally. Each Gateway gets its own Deployment and Service, isolating Gateways in separate network namespaces.

**Unprivileged binds (no `NET_BIND_SERVICE`).** A published listener port (e.g. 80/443) is a client-facing contract, not the port the process must bind. Under `portail-operator`, the Service maps each published port to an unprivileged `targetPort` (e.g. `80 → 8080`) and portail binds that target — so the pod drops *all* capabilities, runs `runAsNonRoot` with `allowPrivilegeEscalation: false` and no sysctl, fully compliant with restricted Pod Security on both vanilla Kubernetes and OpenShift `restricted-v2`. portail discovers its fronting Service by the label `portail.epheo.eu/gateway: <gateway-name>` in the Gateway's namespace and binds the numeric `targetPort` of each port matching a listener. If no such Service exists — multi-network mode, the host-networked DaemonSet, or before the operator has created it — portail binds the published port directly. Those modes add `capabilities.add: [NET_BIND_SERVICE]`, which pairs with the `cap_net_bind_service` **file capability** baked into the image (`setcap …=+p`): portail raises it to effective at startup and binds the privileged port while staying **non-root** with `allowPrivilegeEscalation: false` — no sysctl, no root, no custom SCC. (`+p`, not `+ep`, so the same image still execs in service-mode pods that drop the capability.)

When `spec.addresses` is set on a Gateway, listeners bind to that specific IP. If the IP doesn't exist on the pod's interfaces, the Gateway gets `Programmed=False`. The network itself provides isolation: each Deployment only serves Gateways whose addresses are reachable from its pods.

The same pattern works with **NodePort** or **ClusterIP**. Just change `spec.type` in `service.yaml`. See the loadbalancer example's README for details.

See: `examples/kubernetes/loadbalancer/`

## Multi-Network Routing

A Portail pod attached to multiple User Defined Networks (UDN) via Multus, routing between isolated network segments at L4-L7.

```bash
kubectl apply -k examples/kubernetes/multi-network/
```

Each UDN gets its own Gateway with `spec.addresses` set to Portail's IP on that network. Traffic crossing between networks goes through Portail, which applies Gateway API routing rules (HTTPRoute, TCPRoute, UDPRoute).

NAT is implicit: the backend sees Portail's UDN IP as source, not the original client. No iptables or kernel NAT required.

For pure L3 connectivity between subnets (high throughput, ICMP, no L7 policy), use OVN or iptables. Portail complements L3 infrastructure as the L4-L7 policy layer.

See: `examples/kubernetes/multi-network/`

## Container Image

```
ghcr.io/epheo/portail:latest
```

Build from source:

```bash
cargo build --release
podman build -t ghcr.io/epheo/portail:latest -f Containerfile .
```

## Probes and Metrics

In Kubernetes mode portail serves a plain-HTTP admin endpoint on
`--readiness-port` (default `19099`): `/livez` answers "should this process
be restarted" and is `200` once the process is up; `/readyz` turns `200`
only when the data plane has bound its listener ports; `/metrics` is
Prometheus text format. Point `readinessProbe` at `/readyz` and
`livenessProbe` at `/livez`, always via the pod IP — probing through a
Gateway VIP has no delivery guarantee to this pod and turns a neighbor's
outage into your restart loop.

## Verifying

```bash
# GatewayClass accepted?
kubectl get gatewayclass portail -o jsonpath='{.status.conditions[?(@.type=="Accepted")].status}'

# Gateway status
kubectl get gateway my-gateway -o yaml
# Look for Accepted: True and Programmed: True
```

## Uninstall

```bash
kubectl delete -k examples/kubernetes/<pattern>/
```
