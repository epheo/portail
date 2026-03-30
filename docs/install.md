# Installation

## Prerequisites

- Kubernetes 1.28+ (tested on MicroShift, should work on any conformant cluster)
- Gateway API CRDs installed (experimental channel recommended)

```bash
kubectl apply -f https://github.com/kubernetes-sigs/gateway-api/releases/download/v1.4.1/experimental-install.yaml
```

## Deployment Patterns

Portail is a single binary. How you deploy it is your choice. Each pattern in `examples/kubernetes/` is a self-contained kustomize overlay.

### DaemonSet (quickstart)

One pod per node with `hostNetwork: true`. Simplest setup, good for single-node and edge.

```bash
kubectl apply -k examples/kubernetes/daemonset/
```

### LoadBalancer Service

One Deployment per Gateway, exposed via a LoadBalancer Service. Requires MetalLB or a cloud LB provider. Best for multi-node production.

```bash
kubectl apply -k examples/kubernetes/loadbalancer/
```

### Multi-Network (experimental)

A Portail pod attached to multiple UDNs via Multus, routing between isolated networks at L4-L7.

```bash
kubectl apply -k examples/kubernetes/multi-network/
```

See each pattern's README for detailed pros, cons, and configuration.

## OpenShift / MicroShift

The DaemonSet pattern requires a SecurityContextConstraints for `hostNetwork` and `NET_BIND_SERVICE`:

```bash
kubectl apply -f examples/kubernetes/base/scc.yaml
```

The Deployment-based loadbalancer pattern doesn't need this.

## Container Image

The pre-built image is available on GitHub Container Registry:

```
ghcr.io/epheo/portail:latest
```

Build from source:

```bash
cargo build --release
podman build -t ghcr.io/epheo/portail:latest -f Containerfile .
```

## Verifying

Check that the GatewayClass is accepted:

```bash
kubectl get gatewayclass portail -o jsonpath='{.status.conditions[?(@.type=="Accepted")].status}'
# True
```

Check your Gateway status:

```bash
kubectl get gateway my-gateway -o yaml
```

Look for `Accepted: True` and `Programmed: True` in the conditions.

## Uninstall

```bash
kubectl delete -k examples/kubernetes/<pattern>/
```
