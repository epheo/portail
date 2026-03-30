# DaemonSet with Host Networking

One Portail pod per node, binding directly to the node's network interfaces.

```bash
kubectl apply -k .
```

## When to use

- Single-node clusters
- Edge deployments
- Development and testing
- When you need direct access to host network interfaces

## Pros

- Simplest setup, no LoadBalancer or Service required
- Direct port binding on the node (low latency)
- All Gateways handled by every node

## Cons

- Not multi-node HA (each node is independent, no shared VIP)
- Requires `hostNetwork: true` and `NET_BIND_SERVICE` capability
- Port conflicts if multiple Gateways use the same port
