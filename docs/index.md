# Portail

Portail is a Kubernetes [Gateway API](https://gateway-api.sigs.k8s.io/) controller written in Rust.

It runs as a DaemonSet with `hostNetwork: true`, binding directly to node ports — no Service or LoadBalancer indirection. A single binary handles both the control plane (watching Gateway API resources) and the data plane (proxying traffic).

## Gateway API Conformance

Supported features:

- **HTTP routing**: path prefix/exact, header, query parameter, and method matching
- **Traffic management**: weighted backend routing, request/backend timeouts
- **Header modification**: request and response header add/set/remove, backend request headers
- **URL manipulation**: path rewrite, prefix replace, hostname rewrite, path/port/scheme redirect
- **Request mirroring**: single and multiple mirrors
- **TLS**: termination with SNI-based cert selection, passthrough, hot-reload on Secret changes
- **Protocol support**: HTTP/1.1, TCP, TLS, UDP routing; WebSocket upgrades
- **Multi-tenancy**: listener hostname and port isolation, cross-namespace routing with ReferenceGrant
- **Backend types**: ClusterIP, headless (EndpointSlice), ExternalName services
- **Named route rules**

## Quick Start

```bash
# Install Gateway API CRDs
kubectl apply -f https://github.com/kubernetes-sigs/gateway-api/releases/download/v1.4.1/experimental-install.yaml

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

# Validate without running
portail --config config.yaml --validate-only

# Inspect parsed config
portail --config config.yaml --check-config

# Run with config file
portail --config config.yaml
```

## Links

- [Installation Guide](install.md)

## License

Apache-2.0
