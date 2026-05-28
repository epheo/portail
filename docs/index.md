# Portail Documentation

Portail is a Kubernetes Gateway API implementation. Control plane and data
plane are a single Rust binary; two deployment shapes are supported:

- **Per-Gateway via [portail-operator](https://github.com/epheo/portail-operator)**
  (recommended): the operator provisions one portail Deployment per Gateway,
  scoped to that Gateway via `--gateway namespace/name`. Operator owns
  Gateway/GatewayClass lifecycle status; portail owns per-listener and route
  status. Blast radius per Gateway, K8s-native scaling and resource limits per
  Gateway.
- **Unscoped / manual**: a single portail Deployment or DaemonSet watching all
  Gateways cluster-wide. Simpler to deploy, less isolated; recommended for
  small homelabs and single-Gateway setups.

In both shapes the binary stays the same — only the `--gateway` flag and the
provisioner change.

- [Deployment](deployment.md) — deployment patterns, networking, installation
- [Configuration](configuration.md) — CLI flags, standalone mode, TLS
