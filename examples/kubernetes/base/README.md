# Base Resources

Shared Kubernetes resources required by all deployment patterns:

- `namespace.yaml` — the `portail-system` namespace
- `rbac.yaml` — ServiceAccount, ClusterRole, and ClusterRoleBinding
- `gatewayclass.yaml` — the `portail` GatewayClass

Each deployment pattern overlay adds its own workload, security, and networking resources.
