# Gateway on a Primary UDN

On OVN-Kubernetes, a pod in a namespace with a Primary User Defined Network reaches the apiserver and DNS on the default network, and nothing else: not the controller Service.
Its data plane polls the controller through the apiserver's service proxy instead.

The namespace, `demo-primary` here, must exist with its Primary UDN:

```yaml
apiVersion: v1
kind: Namespace
metadata:
  name: demo-primary
  labels:
    k8s.ovn.org/primary-user-defined-network: ""
---
apiVersion: k8s.ovn.org/v1
kind: UserDefinedNetwork
metadata:
  name: primary
  namespace: demo-primary
spec:
  topology: Layer2
  layer2:
    role: Primary
    subnets: ["10.100.0.0/16"]
```

Then, with the controller installed:

```bash
kubectl apply -k .
kubectl -n demo-primary get gateway svc-gw
```

- `gatewayparameters.yaml`: `controllerTransport: APIServerProxy`. The pod gets `--transport apiserver-proxy` and an API-audience token beside its controller token.
- `rolebinding.yaml`: binds Role `portail-controller-proxy` to the Gateway's ServiceAccount, `portail-svc-gw`. One subject per Gateway that polls this way.
- `deploy/components/apiserver-proxy`, as a component: the Role, `services/proxy` on `https:portail-controller:9443`, verb `create`.

All proxied polls reach the controller from each apiserver node's management address: with more than 16 such data planes per apiserver node, raise the controller's `--sync-peer-connections` (default 32) to twice their number.

Without the RoleBinding the data plane logs `apiserver answered 403 Forbidden ... configuration error` and keeps polling; nothing it serves changes.

See [deployment](../../../docs/deployment.md#primary-udn) for the scale envelope and the trust model.
