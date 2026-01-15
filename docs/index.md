# Portail

Portail is a Kubernetes [Gateway API](https://gateway-api.sigs.k8s.io/) controller written in Rust.

It runs as a DaemonSet with `hostNetwork: true`, binding directly to node ports — no Service or LoadBalancer indirection. A single binary handles both the control plane (watching Gateway API resources) and the data plane (proxying traffic).

## Conformance

**346 / 383 tests passing** (90%) against the official Gateway API conformance suite.

Supported features:

- HTTP routing (path, header, query param, method matching)
- Request/response header modification
- URL rewrite and redirect
- Request mirroring (single and multiple mirrors)
- Weighted backend routing
- Request and backend timeouts
- TLS termination and passthrough (SNI-based)
- TCP and UDP routing
- Listener hostname isolation
- Port-based listener isolation
- Cross-namespace routing with ReferenceGrant
- Named route rules

## Quick Start

```bash
# Install Gateway API CRDs
kubectl apply -f https://github.com/kubernetes-sigs/gateway-api/releases/download/v1.2.1/experimental-install.yaml

# Deploy Portail
kubectl apply -f deploy/namespace.yaml
kubectl apply -f deploy/rbac.yaml
kubectl apply -f deploy/gatewayclass.yaml
kubectl apply -f deploy/daemonset.yaml
```

Then create a Gateway and HTTPRoute:

```yaml
apiVersion: gateway.networking.k8s.io/v1
kind: Gateway
metadata:
  name: my-gateway
  namespace: default
spec:
  gatewayClassName: portail
  listeners:
  - name: http
    port: 80
    protocol: HTTP
---
apiVersion: gateway.networking.k8s.io/v1
kind: HTTPRoute
metadata:
  name: my-route
  namespace: default
spec:
  parentRefs:
  - name: my-gateway
  hostnames:
  - "app.example.com"
  rules:
  - matches:
    - path:
        type: PathPrefix
        value: /
    backendRefs:
    - name: my-service
      port: 8080
```

## Standalone Mode

Portail can also run without Kubernetes, loading routes from a config file:

```bash
# Generate a starter config
portail --generate-config development --output config.yaml

# Run with config file
portail --config config.yaml
```

## Links

- [Installation Guide](install.md)
- [Configuration Examples](configuration.md)
- [Architecture](architecture.md)

## License

Apache-2.0
