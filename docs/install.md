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

## Container Image

Build from source:

```bash
cargo build --release
podman build -t portail:latest -f Containerfile .
```

Or use a pre-built image from the registry:

```yaml
# In deploy/daemonset.yaml, set the image:
image: quay.io/epheo/portail:latest
```

## Network Model

Portail uses `hostNetwork: true` — the pod binds directly to the node's network interfaces. Ports defined in your Gateway listeners (e.g., 80, 443, 8080) are opened on the node. No Kubernetes Service or LoadBalancer is needed.

The `NET_BIND_SERVICE` capability allows binding to privileged ports (< 1024) without running as root.

## Firewall

If your node has a firewall, open the ports your Gateway listeners use:

```bash
sudo firewall-cmd --add-port=80/tcp --permanent
sudo firewall-cmd --add-port=443/tcp --permanent
sudo firewall-cmd --reload
```

## TLS Certificates

Portail reads TLS certificates from Kubernetes Secrets referenced in your Gateway listener's `certificateRefs`. It works with cert-manager or any other Secret-based certificate management.

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
