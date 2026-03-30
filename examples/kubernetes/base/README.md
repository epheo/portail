# Base Resources

Shared Kubernetes resources required by all deployment patterns:

## OpenShift / MicroShift

On OpenShift-based clusters, apply the SecurityContextConstraints for hostNetwork and NET_BIND_SERVICE:

```bash
kubectl apply -f scc.yaml
```

This is only needed for the `daemonset/` pattern. Deployment-based patterns don't require hostNetwork.
