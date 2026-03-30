# Multi-Network Routing (Experimental)

A single Portail pod attached to multiple User Defined Networks (UDN) via Multus, routing traffic between them at L4-L7.

```bash
kubectl apply -k .
```

## When to use

- VPC-style multi-tenant networking
- Inter-UDN routing with L4-L7 policy
- KubeVirt environments with isolated VM networks

## Architecture

```
+--------------+    +--------------+
| UDN-frontend |    | UDN-backend  |
| 10.10.1.0/24 |    | 10.10.2.0/24 |
|              |    |              |
| web VMs/pods |    | app VMs/pods |
+------+-------+    +------+-------+
       |                   |
       +--------+----------+
                |
      +---------+----------+
      |  Portail pod       |
      |  10.10.1.1 (fe)    |
      |  10.10.2.1 (be)    |
      |                    |
      |  Gateway API rules |
      |  decide what flows |
      |  between networks  |
      +--------------------+
```

Each UDN gets its own Gateway with `spec.addresses` set to Portail's IP on that network. Routes attached to a Gateway define what traffic can cross from that network to backends on other networks.

## How it works

- **NAT is implicit**: Portail accepts on one UDN interface, connects to the backend on another. The backend sees Portail's IP as source.
- **Network attachment is the security boundary**: the pod is only attached to the UDNs listed in the Multus annotation. It physically cannot route to other networks.
- **L4-L7 policy**: HTTPRoutes give you path/header-based routing, TLS termination, mirroring. TCPRoutes and UDPRoutes give you raw L4 forwarding (SSH, databases, etc.).

## Complementary to L3 infrastructure

Pure L3 subnet routing (forward any packet between subnets) belongs to OVN/iptables -- kernel-level, no per-connection overhead. Portail handles the L4-L7 layer on top: routing decisions, TLS, load balancing, observability.

Use both: OVN for baseline connectivity, Portail when you need application-level control.

## Prerequisites

- Multus CNI or UDN support in your cluster
- Network attachment definitions for each UDN
