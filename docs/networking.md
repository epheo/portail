# Network Model

Portail supports multiple networking patterns depending on how you deploy it.

## DaemonSet with Host Networking

Portail uses `hostNetwork: true` -- the pod binds directly to the node's network interfaces. Ports defined in your Gateway listeners (e.g., 80, 443) are opened on the node. No Kubernetes Service is needed.

The `NET_BIND_SERVICE` capability allows binding to privileged ports (< 1024) without running as root.

If your node has a firewall, open the ports your Gateway listeners use:

```bash
sudo firewall-cmd --add-port=80/tcp --permanent
sudo firewall-cmd --add-port=443/tcp --permanent
sudo firewall-cmd --reload
```

See: `examples/kubernetes/daemonset/`

## Deployment with Service

Portail runs as a regular Deployment (no `hostNetwork`). A Kubernetes Service exposes it externally. Each Gateway gets its own Deployment and Service, isolating Gateways in separate network namespaces.

Available Service types:

- **LoadBalancer** -- MetalLB or cloud CCM assigns a VIP. Best for production. See: `examples/kubernetes/loadbalancer/`
- **NodePort** or **ClusterIP** -- Same pattern, just change `spec.type` on the Service. NodePort exposes on every node at a high port (30000+); ClusterIP is internal only.

Portail reconciles all Gateways for its GatewayClass. When `spec.addresses` is set, listeners bind to that specific IP. If the IP doesn't exist on the pod's interfaces, the Gateway gets `Programmed=False`. The network itself provides isolation -- each Deployment only serves Gateways whose addresses are reachable from its pods.

## Multi-Network Routing

A Portail pod attached to multiple User Defined Networks (UDN) via Multus routes traffic between isolated network segments at L4-L7.

Each UDN gets its own Gateway with `spec.addresses` set to Portail's IP on that network. Traffic crossing between networks goes through Portail, which applies Gateway API routing rules (HTTPRoute, TCPRoute, UDPRoute).

NAT is implicit: the backend sees Portail's UDN IP as source, not the original client. No iptables or kernel NAT required.

For pure L3 connectivity between subnets (high throughput, ICMP, no L7 policy), use OVN or iptables. Portail complements L3 infrastructure as the L4-L7 policy layer.

See: `examples/kubernetes/multi-network/`
