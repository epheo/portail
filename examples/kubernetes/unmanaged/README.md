# Unmanaged Gateway with a hand-written Deployment

The controller serves the Gateway's configuration and writes its status; the pods are yours.
Use it for what the controller will not render: `hostNetwork`, init containers with `NET_ADMIN`, a DaemonSet, an SCC.

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
kubectl -n portail-system get gateway edge
```

`deployment.yaml` runs the release `deploy/install` names; with the main override, set its image to `ghcr.io/epheo/portail:sha-<full commit sha>` too.

- `gatewayparameters.yaml`: `provisioning: Unmanaged`; the controller creates and deletes nothing for Gateways using it.
- `serviceaccount.yaml`: `portail-<gateway>` in the Gateway's namespace, the only identity the controller serves this Gateway to.
- `deployment.yaml`: `portail dataplane` with a projected token of the controller audience and the controller CA.
- `httproute.yaml`: routes `example.com` to `my-service:8080`.

## What a data plane needs

- ServiceAccount `portail-<gateway>` in the Gateway's namespace; `automountServiceAccountToken: false`.
- A projected token with audience `portail-controller.<controller namespace>.svc` at `/var/run/secrets/portail.epheo.eu/controller/token`.
- The controller CA: `--controller-ca-file`, or its PEM in env `PORTAIL_CONTROLLER_CA`.
- `--gateway <namespace>/<name>` and `--controller-address https://portail-controller.<controller namespace>.svc:9443`.
- The controller's image: data planes upgrade with it.

Each pod reports to the controller; `Programmed` needs one live pod at the current configuration.
`status.addresses` lists what the pods report: the `IPAddress` values of `spec.addresses` found on a pod, resolved Network addresses, else each pod's first interface IP.

## The controller CA outside its namespace

The controller publishes its CA in ConfigMap `portail-controller-ca` of its own namespace.
A Gateway in another namespace needs a copy there, refreshed after a CA rotation:

```bash
kubectl -n portail-system get configmap portail-controller-ca -o jsonpath='{.data.ca\.crt}' > ca.crt
kubectl -n <gateway namespace> create configmap portail-controller-ca --from-file=ca.crt
```

## Host network

To bind the node's interfaces, one pod per node:

- make it a DaemonSet;
- set `hostNetwork: true` and `dnsPolicy: ClusterFirstWithHostNet`, so the pod still resolves the controller Service;
- open the listener ports in the node firewall.

Pod Security `baseline` and `restricted` refuse `hostNetwork`: label the namespace `privileged`, or on OpenShift grant an SCC allowing host network and `NET_BIND_SERVICE` to `system:serviceaccount:portail-system:portail-edge`.
