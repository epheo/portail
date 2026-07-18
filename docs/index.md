# Portail Documentation

Portail is a Kubernetes Gateway API implementation.
Control plane and data plane are a single Rust binary -- no sidecar,
self-sufficient standalone or cluster-wide, or provisioned as per-Gateway
data planes by [portail-operator](https://github.com/epheo/portail-operator).
How you deploy it is your business.

- [Deployment](deployment.md) -- deployment patterns, networking, installation
- [Configuration](configuration.md) -- CLI flags, standalone mode, TLS
