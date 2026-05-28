# Deployment

## Prerequisites

- Kubernetes 1.28+ (tested on MicroShift, should work on any conformant cluster)
- Gateway API CRDs installed (experimental channel recommended)

```bash
kubectl apply -f https://github.com/kubernetes-sigs/gateway-api/releases/download/v1.4.1/experimental-install.yaml
```

## Recommended: Per-Gateway via portail-operator

For multi-Gateway production setups, deploy [portail-operator](https://github.com/epheo/portail-operator).
It watches `Gateway` resources and provisions one portail Deployment (+ ServiceAccount,
RBAC, Service) per Gateway, passing `--gateway namespace/name` so each pod reconciles
only its own Gateway.

Properties this gives you:

- **Status ownership is unambiguous.** The operator writes
  Gateway/GatewayClass conditions and `status.addresses`; portail writes only
  per-listener status and route status. No multi-writer SSA fights.
- **Per-Gateway isolation.** Each Gateway has its own pod, network namespace,
  resource limits, replicas, and crash blast radius.
- **K8s-native scaling.** Standard Deployment knobs apply per Gateway.
- **`spec.addresses` → MetalLB.** When a Gateway carries an `IPAddress` in
  `spec.addresses`, the operator wires it onto the generated Service via
  `loadBalancerIP` + the `metallb.universe.tf/loadBalancerIPs` annotation.

Tradeoffs: one Deployment per Gateway means more pods at scale, and new-Gateway
latency is the K8s pod startup time (≈5–15s) rather than near-instant. New
Gateway provisioning depends on the operator being healthy.

Install steps live in the [operator README](https://github.com/epheo/portail-operator).

The patterns below are for **unscoped** deployment without the operator — one portail
process watches all Gateways cluster-wide. Simpler to deploy, less isolated;
appropriate for single-Gateway setups, homelabs, or edge.

## Without operator: DaemonSet with Host Networking

One pod per node with `hostNetwork: true`. Simplest setup, good for single-node and edge.

```bash
kubectl apply -k examples/kubernetes/daemonset/
```

The pod binds directly to the node's network interfaces. Ports defined in your Gateway listeners (e.g., 80, 443) are opened on the node. No Kubernetes Service is needed. `NET_BIND_SERVICE` allows binding to privileged ports (< 1024) without running as root.

If your node has a firewall, open the ports your Gateway listeners use:

```bash
sudo firewall-cmd --add-port=80/tcp --add-port=443/tcp --permanent && sudo firewall-cmd --reload
```

On OpenShift / MicroShift, apply the SecurityContextConstraints for `hostNetwork` and `NET_BIND_SERVICE`:

```bash
kubectl apply -f examples/kubernetes/daemonset/scc.yaml
```

See: `examples/kubernetes/daemonset/`

## Without operator: Deployment with LoadBalancer Service

A single portail Deployment watching all Gateways, exposed via a LoadBalancer Service. Requires MetalLB or a cloud LB provider. Suitable for small clusters with one or two Gateways where the operator would be overkill.

```bash
kubectl apply -k examples/kubernetes/loadbalancer/
```

Portail runs as a regular Deployment (no `hostNetwork`). A Kubernetes Service exposes it externally. The Deployment serves all Gateways its watch sees; isolate Gateways by deploying to separate namespaces or by switching to the operator path above.

When `spec.addresses` is set on a Gateway, listeners bind to that specific IP. If the IP doesn't exist on the pod's interfaces, the Gateway gets `Programmed=False`. The network itself provides isolation: each Deployment only serves Gateways whose addresses are reachable from its pods.

The same pattern works with **NodePort** or **ClusterIP**. Just change `spec.type` in `service.yaml`. See the loadbalancer example's README for details.

See: `examples/kubernetes/loadbalancer/`

## Without operator: Multi-Network Routing

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
