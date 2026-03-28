# Installation

## Prerequisites

- Kubernetes 1.28+ (tested on MicroShift, should work on any conformant cluster)
- Gateway API CRDs installed (experimental channel recommended)

```bash
kubectl apply -f https://github.com/kubernetes-sigs/gateway-api/releases/download/v1.4.1/experimental-install.yaml
```

## Deploy

Apply the manifests in order:

```bash
kubectl apply -f deploy/namespace.yaml
kubectl apply -f deploy/rbac.yaml
kubectl apply -f deploy/gatewayclass.yaml
kubectl apply -f deploy/daemonset.yaml
```

This creates:

- `portail-system` namespace
- A `ServiceAccount` with RBAC to read Gateway API resources and update their status
- A `GatewayClass` named `portail` with controller name `portail.epheo.eu/gateway-controller`
- A `DaemonSet` running one pod per node with `hostNetwork: true`

### OpenShift / MicroShift

On OpenShift-based clusters, the default security policy prevents `hostNetwork` and `NET_BIND_SERVICE`. Apply the SecurityContextConstraints:

```bash
kubectl apply -f deploy/scc.yaml
```

This grants the `portail-controller` service account permission to use host networking and bind to privileged ports.

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
kubectl delete -f deploy/daemonset.yaml
kubectl delete -f deploy/gatewayclass.yaml
kubectl delete -f deploy/rbac.yaml
kubectl delete -f deploy/namespace.yaml
```
