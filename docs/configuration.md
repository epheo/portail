# Configuration

## CLI Reference

| Flag | Description |
|------|-------------|
| `--kubernetes` | Run as Kubernetes Gateway API controller |
| `--controller-name <NAME>` | Controller name for GatewayClass matching (default: `portail.epheo.eu/gateway-controller`) |
| `--config, -c <FILE>` | Load configuration from a JSON or YAML file (mutually exclusive with `--kubernetes`) |
| `--validate-only` | Validate config file without starting the server (requires `--config`) |
| `--check-config` | Parse and display config values, then exit (requires `--config`) |
| `--generate-config <TYPE>` | Generate example config: `minimal` or `development` |
| `--output <FILE>` | Output file for generated configuration (requires `--generate-config`) |
| `--cert-dir <DIR>` | Directory for TLS certificate files (default: `/etc/portail/certs`) |
| `--supported-features` | Print supported Gateway API features and exit |
| `--verbose, -v` | Enable verbose logging (repeat for more: `-vv`, `-vvv`) |

## Custom Controller Name

To use a custom controller name (e.g., for running multiple Portail instances):

```bash
portail --kubernetes --controller-name my-org.example.com/gateway-controller
```

Update the `GatewayClass` to match:

```yaml
apiVersion: gateway.networking.k8s.io/v1
kind: GatewayClass
metadata:
  name: portail
spec:
  controllerName: my-org.example.com/gateway-controller
```

## Standalone Mode

Portail can run without Kubernetes, loading routes from a config file:

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

## TLS Certificates

### Kubernetes Mode

Portail reads TLS certificates from Kubernetes Secrets referenced in your Gateway listener's `certificateRefs`. It works with cert-manager or any other Secret-based certificate management.

TLS certificates are hot-reloaded when the underlying Secret changes — no restart required. Multiple Gateways sharing a port have their certificates merged automatically for SNI-based selection.

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

### Standalone Mode

In standalone mode, TLS certificates are read from the filesystem. Place `{name}.crt` and `{name}.key` files in the cert directory:

```bash
portail --config config.yaml --cert-dir /path/to/certs
```

The default cert directory is `/etc/portail/certs`.
