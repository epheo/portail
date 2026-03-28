# Network Model

## Host Networking

Portail uses `hostNetwork: true` — the pod binds directly to the node's network interfaces. Ports defined in your Gateway listeners (e.g., 80, 443, 8080) are opened on the node. No Kubernetes Service or LoadBalancer is needed.

The `NET_BIND_SERVICE` capability allows binding to privileged ports (< 1024) without running as root.

## Firewall

If your node has a firewall, open the ports your Gateway listeners use:

```bash
sudo firewall-cmd --add-port=80/tcp --permanent
sudo firewall-cmd --add-port=443/tcp --permanent
sudo firewall-cmd --reload
```
