# Portail Documentation

Portail is a Kubernetes Gateway API implementation in one Rust binary:
a leader-elected controller, one data plane per Gateway that the controller
provisions or that you run yourself, or a standalone proxy fed by a file.

- [Deployment](deployment.md) -- installation, GatewayParameters, networking, failure behavior
- [Configuration](configuration.md) -- CLI flags, standalone mode, TLS
- [Upgrading](upgrading.md) -- from 0.1.x to 0.2.0
