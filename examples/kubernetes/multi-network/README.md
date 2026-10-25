# Multi-Network Routing

A Gateway whose data plane sits on several User Defined Networks (UDN) and routes between them at L4-L7 with standard Gateway API resources.

## Architecture

```
  Tenant A                              Tenant B
 +----------------------+              +----------------------+
 |  UDN-frontend        |              |  UDN-backend         |
 |  10.x.1.0/24         |              |  10.x.2.0/24         |
 |                      |              |                      |
 |  web VMs / pods      |              |  databases / pods    |
 +----------+-----------+              +----------+-----------+
            |                                     |
            +-----------------+-------------------+
                              |
                    +---------+----------+
                    |  portail data      |
                    |  plane             |
                    |  net1: 10.x.1.y    |
                    |  net2: 10.x.2.y    |
                    |                    |
                    |  HTTPRoute /api/*  |
                    |  TCPRoute  :5432   |
                    +--------------------+
```

The data plane straddles network boundaries and applies Gateway API routing rules at the seam.
Network attachment is the security boundary: the pod cannot route to networks it is not attached to.
Routes define what crosses, at L4-L7.

## Network addresses

`portail.epheo.eu/Network` is a custom Gateway address type naming a network instead of an IP:

```yaml
spec:
  addresses:
  - type: portail.epheo.eu/Network
    value: udn-frontend
```

For such a Gateway the controller:

- attaches the data-plane pod to the named network through Multus;
- creates no Service, unless GatewayParameters set `service.type`;
- reads the NetworkAttachmentDefinition's subnet, which the data plane matches against its interfaces to bind that IP only;
- reports the bound IP in `status.addresses`.

If the network has no IP on the pod, nothing binds and `Programmed` is `False` with reason `AddressNotUsable`.
GatewayParameters `networks` attach further networks, as both examples do for the backend side.
The data plane binds published ports directly there: a port below 1024 gets `NET_BIND_SERVICE` added, and nothing else.

## Examples

Install the controller once per cluster, at a release tag:

```bash
kubectl apply --server-side -k "https://github.com/epheo/portail//deploy/install?ref=vX.Y.Z"
```

At a commit of main, override its image in a kustomization of your own, as in [the examples README](../README.md):

```yaml
resources:
- https://github.com/epheo/portail//deploy/install?ref=<full commit sha>
images:
- name: ghcr.io/epheo/portail
  newTag: sha-<full commit sha>
```

### [http-single-namespace/](http-single-namespace/)

HTTP between two UDNs in one namespace.

- Two `UserDefinedNetwork` resources (Layer2, secondary).
- Gateway `gw-frontend` on `udn-frontend`, also attached to `udn-backend`.
- An HTTPRoute forwarding all traffic to `backend-service`.

### [tcp-cross-namespace/](tcp-cross-namespace/)

TCP (PostgreSQL) across namespaces, with explicit cross-namespace authorization.

- Two `ClusterUserDefinedNetwork` resources, attached by the labeled tenant namespaces.
- Gateway `gw-frontend` in `tenant-frontend`, on `vpc-frontend`, also attached to `vpc-backend`.
- A TCPRoute forwarding port 5432 to `postgres` in `tenant-backend`.
- A ReferenceGrant in `tenant-backend` permitting that backendRef.

## Complementary to L3 infrastructure

Pure L3 subnet routing belongs to OVN or iptables: kernel-level, no per-connection overhead.
portail handles the L4-L7 layer on top: routing decisions, TLS, load balancing, observability.

## Prerequisites

- OpenShift 4.16+ or Kubernetes with OVN-Kubernetes and UDN support.
- The UserDefinedNetwork and ClusterUserDefinedNetwork CRDs (`k8s.ovn.org/v1`).
- The TCPRoute CRD for the TCP example (experimental Gateway API channel).
