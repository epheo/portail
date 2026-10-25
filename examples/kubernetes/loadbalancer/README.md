# Gateway behind a LoadBalancer Service

A Managed Gateway with two data-plane replicas.
The controller creates `portail-example-gw`: a ServiceAccount, a Deployment, a LoadBalancer Service and a PodDisruptionBudget.

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

Then:

```bash
kubectl apply -k .
kubectl get gateway example-gw
```

- `gatewayparameters.yaml`: `replicas: 2`; the Service type defaults to LoadBalancer.
- `gateway.yaml`: references it through `spec.infrastructure.parametersRef`.
- `httproute.yaml`: routes `example.com` to `my-service:8080`.
- `metallb-pool.yaml`: an address pool for MetalLB, if the cluster has no other LoadBalancer provider; not applied by default.

## Ports and privileges

The Service publishes 80 and 443.
The pods bind unprivileged target ports (8000 and up) instead, so they run with every capability dropped.

## Addresses

`status.addresses` is the LoadBalancer ingress the provider assigns.
An `IPAddress` in `spec.addresses` is not requested from the provider: `Programmed` is `False` with reason `AddressNotAssigned` unless it assigns that address anyway.

## NodePort or ClusterIP

Set `service.type` in `gatewayparameters.yaml`:

```yaml
spec:
  replicas: 2
  service:
    type: ClusterIP   # or NodePort
```

`status.addresses` is then the Service's cluster IP.

## MetalLB

Edit the range in `metallb-pool.yaml` to free addresses of your network, then apply it.
`10.89.0.200-10.89.0.250` fits kind on Podman (`10.89.0.0/24`); on Docker kind uses `172.18.0.0/16`.
