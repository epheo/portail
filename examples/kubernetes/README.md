# Kubernetes examples

Install the controller once per cluster, at a release tag:

```bash
kubectl apply --server-side -k "https://github.com/epheo/portail//deploy/install?ref=vX.Y.Z"
```

At a commit of main, `deploy/install` still names the last release's image, which may lack the `controller` subcommand.
Install the commit's own image with a kustomization of your own, applied with `kubectl apply --server-side -k <its directory>`:

```yaml
apiVersion: kustomize.config.k8s.io/v1beta1
kind: Kustomization
resources:
- https://github.com/epheo/portail//deploy/install?ref=<full commit sha>
images:
- name: ghcr.io/epheo/portail
  newTag: sha-<full commit sha>
```

Then apply an example with `kubectl apply -k <dir>`:

- [loadbalancer/](loadbalancer/): a Managed Gateway with two replicas behind a LoadBalancer Service.
- [multi-network/](multi-network/): Gateways on User Defined Networks, by `portail.epheo.eu/Network` address.
- [unmanaged/](unmanaged/): an Unmanaged Gateway served by a hand-written Deployment.
- [ratelimitpolicy.yaml](ratelimitpolicy.yaml): a per-client rate limit on the unmanaged example's Gateway.

See [deployment](../../docs/deployment.md) for GatewayParameters and the controller.
