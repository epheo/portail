# Deployment with LoadBalancer Service

One Deployment per Gateway, exposed via a Service(type=LoadBalancer). A LoadBalancer provider (MetalLB, cloud CCM) assigns a VIP.

```bash
kubectl apply -k .
```

## When to use

- Multi-node production clusters
- When you need HA with multiple replicas
- When you have MetalLB or a cloud LoadBalancer provider

## Pros

- HA: multiple replicas spread across nodes
- No `hostNetwork` required, no special capabilities
- No port conflicts between Gateways (each has its own pods)
- Kubernetes-native: standard Deployment + Service pattern
- Each Gateway is isolated in its own network namespace

## Cons

- One Deployment + Service per Gateway

## MetalLB

This example includes MetalLB. Edit `metallb-pool.yaml` to match your network. The default range `10.89.0.200-10.89.0.250` works with Kind's Podman network (`10.89.0.0/24`). Docker uses `172.18.0.0/16`, adjust to e.g. `172.18.255.200-172.18.255.250`. The conformance test computes this automatically.

If using a cloud provider with its own LoadBalancer controller, remove the MetalLB resources from `kustomization.yaml`.

## Multi-interface / MetalLB address pools

To bind different Gateways to different network interfaces, use MetalLB address pools:

```yaml
# On the Service:
metadata:
  annotations:
    metallb.universe.tf/address-pool: "public-pool"
```

Create separate Gateways for each interface, each with its own Deployment and Service pointing to a different MetalLB pool.

## How isolation works

Portail reconciles all Gateways for its GatewayClass. When `spec.addresses` is set, listeners bind to that specific IP. If the IP doesn't exist on the pod's interfaces, the Gateway gets `Programmed=False` and is effectively ignored. The network is the filter.

## Using NodePort or ClusterIP instead

This same Deployment + Service pattern works with any Service type. Change `spec.type` in `service.yaml`:

- `NodePort` -- exposes on every node at a high port (30000+), works without a LB provider
- `ClusterIP` -- cluster-internal only, for internal API gateways and inter-namespace routing

If using NodePort or ClusterIP, remove the MetalLB resources from `kustomization.yaml`.

## Configuration

- `spec.addresses` on the Gateway, set to the VIP assigned by your LoadBalancer provider
- `replicas` scale as needed for throughput and availability
