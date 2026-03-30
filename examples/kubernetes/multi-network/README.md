# Multi-Network Routing

A single Portail pod attached to multiple User Defined Networks (UDN), routing traffic between them at L4-L7 using standard Gateway API resources.

## Architecture

```
  Tenant A                              Tenant B
 ┌──────────────────────┐              ┌──────────────────────┐
 │  UDN-frontend        │              │  UDN-backend         │
 │  10.x.1.0/24         │              │  10.x.2.0/24         │
 │                      │              │                      │
 │  web VMs / pods      │              │  databases / pods    │
 └──────────┬───────────┘              └──────────┬───────────┘
            │                                     │
            └───────────────┬─────────────────────┘
                            │
                  ┌─────────┴──────────┐
                  │  Portail           │
                  │  net1: 10.x.1.y    │
                  │  net2: 10.x.2.y    │
                  │                    │
                  │  HTTPRoute /api/*  │
                  │  TCPRoute  :5432   │
                  │  TLSRoute  *.db    │
                  │  UDPRoute  :53     │
                  └────────────────────┘
```

Portail straddles network boundaries and applies Gateway API routing rules at the seam. Network attachment is the security boundary — the pod physically cannot route to networks it is not attached to. Routes define what crosses, at L4-L7.

## Custom address type

Portail introduces `portail.epheo.eu/Network` as a custom Gateway address type. It binds a Gateway to a named network without hardcoding IPs:

```yaml
spec:
  addresses:
  - type: portail.epheo.eu/Network
    value: udn-frontend
```

Portail reads the NetworkAttachmentDefinition to get the subnet CIDR, matches it against local interface IPs, and reports the resolved IP in `status.addresses`. Addresses update automatically on pod restart.

## Examples

### [http-single-namespace/](http-single-namespace/)

HTTP routing between two UDNs in a single namespace. The simplest setup — good starting point.

- Two `UserDefinedNetwork` resources (Layer2, secondary)
- HTTPRoute forwarding all traffic from frontend to backend
- Tested on OpenShift 4.20 with OVN-Kubernetes

### [tcp-cross-namespace/](tcp-cross-namespace/)

TCP routing (PostgreSQL) across namespace boundaries. Demonstrates multi-tenant network isolation with explicit cross-namespace authorization.

- Two `ClusterUserDefinedNetwork` resources shared across namespaces
- TCPRoute forwarding port 5432 from frontend network to backend namespace
- ReferenceGrant authorizing the cross-namespace backendRef
- Portail in `portail-system`, backend in `tenant-backend`

## How it works

- **Network attachment is the security boundary**: the pod is only attached to the UDNs listed in the Multus annotation. It physically cannot route to other networks.
- **NAT is implicit**: Portail accepts on one UDN interface, connects to the backend on another. The backend sees Portail's IP as source.
- **L4-L7 policy**: HTTPRoutes give you path/header-based routing and TLS termination. TCPRoutes and UDPRoutes give you raw L4 forwarding for databases, SSH, DNS, etc.
- **Cross-tenant**: ReferenceGrants allow backends in other namespaces to be referenced from routes, enabling multi-tenant topologies with explicit authorization.

## Complementary to L3 infrastructure

Pure L3 subnet routing belongs to OVN/iptables — kernel-level, no per-connection overhead. Portail handles the L4-L7 layer on top: routing decisions, TLS, load balancing, observability.

Use both: OVN for baseline connectivity, Portail when you need application-level control.

## Prerequisites

- OpenShift 4.16+ or Kubernetes with OVN-Kubernetes and UDN support
- UserDefinedNetwork or ClusterUserDefinedNetwork CRD (`k8s.ovn.org/v1`)
- TCPRoute CRD for TCP examples (experimental Gateway API)
