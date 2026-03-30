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

## Deployment with LoadBalancer Service

One Deployment per Gateway, exposed via a LoadBalancer Service. Requires MetalLB or a cloud LB provider. Best for multi-node production.

```bash
kubectl apply -k examples/kubernetes/loadbalancer/
```

Portail runs as a regular Deployment (no `hostNetwork`). A Kubernetes Service exposes it externally. Each Gateway gets its own Deployment and Service, isolating Gateways in separate network namespaces.

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
