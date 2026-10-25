# Kubernetes examples

Install the controller once per cluster:

```bash
kubectl apply --server-side -k ../../deploy/install
```

Then apply an example with `kubectl apply -k <dir>`:

- [loadbalancer/](loadbalancer/): a Managed Gateway with two replicas behind a LoadBalancer Service.
- [multi-network/](multi-network/): Gateways on User Defined Networks, by `portail.epheo.eu/Network` address.
- [unmanaged/](unmanaged/): an Unmanaged Gateway served by a hand-written Deployment.
- [ratelimitpolicy.yaml](ratelimitpolicy.yaml): a per-client rate limit on the unmanaged example's Gateway.

See [deployment](../../docs/deployment.md) for GatewayParameters and the controller.
