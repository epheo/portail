# Deployment

## Prerequisites

- Kubernetes 1.28+, OpenShift and MicroShift included.
- Gateway API CRDs v1.4.1 (experimental channel for TCPRoute, TLSRoute and UDPRoute):

```bash
kubectl apply --server-side -f https://github.com/kubernetes-sigs/gateway-api/releases/download/v1.4.1/experimental-install.yaml
```

## Install

Install a release, from its asset:

```bash
kubectl apply --server-side -f https://github.com/epheo/portail/releases/download/vX.Y.Z/install.yaml
```

Or from the repository at its tag:

```bash
kubectl apply --server-side -k "https://github.com/epheo/portail//deploy/install?ref=vX.Y.Z"
kubectl -n portail-system rollout status deploy/portail-controller
```

`deploy/install` names the image of the release that last set it.
At any other commit it runs an older image, possibly one without the `controller` subcommand.
A commit of main has its own image, `:sha-<full commit sha>`: set it in a kustomization of your own, then `kubectl apply --server-side -k <its directory>`:

```yaml
apiVersion: kustomize.config.k8s.io/v1beta1
kind: Kustomization
resources:
- https://github.com/epheo/portail//deploy/install?ref=<full commit sha>
images:
- name: ghcr.io/epheo/portail
  newTag: sha-<full commit sha>
```

That image renders Managed data planes of the same commit.

`deploy/install` creates:

- Namespace `portail-system`.
- The GatewayParameters and RateLimitPolicy CRDs.
- ServiceAccount `portail-controller`, its ClusterRole, its Role in `portail-system`, and their bindings.
- Deployment `portail-controller`, one replica.
- ClusterIP Service `portail-controller`, port 9443 (`sync`), which data planes dial.
- GatewayClass `portail`.

`deploy/ha` is the same with two controller replicas, a PodDisruptionBudget and a soft spread across nodes.
It is for multi-node clusters: on a single node both replicas sit on that node, and the budget of one available blocks its drain.

```bash
kubectl apply --server-side -k "https://github.com/epheo/portail//deploy/ha?ref=vX.Y.Z"
```

Another image, such as a mirror, goes through a kustomization of your own:

```yaml
apiVersion: kustomize.config.k8s.io/v1beta1
kind: Kustomization
resources:
- https://github.com/epheo/portail//deploy/install?ref=vX.Y.Z
images:
- name: ghcr.io/epheo/portail
  newName: registry.example.com/portail
  newTag: X.Y.Z
```

Managed data planes still run the image the controller was built for: set GatewayParameters `image`, or the controller's `--dataplane-image`, to the mirror too.

## Architecture

One image, two Kubernetes modes:

- `portail controller`: watches Gateway API resources cluster-wide, translates each Gateway into its data plane's configuration, provisions data planes and writes every status. Leader-elected: standbys keep their caches warm and write nothing.
- `portail dataplane --gateway <ns>/<name>`: serves one Gateway. It has no Kubernetes client, no RBAC and no API token: it long-polls the controller over TLS, `POST /v1/sync/<ns>/<name>` on port 9443, and reports which listeners it bound. One opt-in exception, for pods that reach only the apiserver: [Primary UDN](#primary-udn).

One data plane serves exactly one Gateway: Gateways never share a route table or a socket.
TLS keys travel only from the controller to the data plane of the Gateway that references them, and stay in its memory.
Only the controller reads Secrets.

A route edit reaches the data planes within about a second or two: the controller debounces watch events for a second, translates, and answers the held polls at once.

## Managed Gateways

By default the controller provisions each Gateway of class `portail`, in the Gateway's namespace, named `portail-<gateway>`:

- a ServiceAccount, without an API token;
- a Deployment running `portail dataplane`;
- a Service, unless its type resolves to `None`;
- a PodDisruptionBudget, with more than one replica.

Each object carries a controller ownerReference to its Gateway: deleting the Gateway garbage-collects them.
Each also carries label `portail.epheo.eu/controller-name`, the install's `--controller-name` as a label value: another portail install, with another controller name, never updates or deletes it.
An object of a wanted name the Gateway does not own is never touched: the Gateway reports `Programmed=False`, reason `Invalid`, `name conflict: <Kind>/<name> is not owned by this Gateway`.
A Gateway whose class changes to another controller's has its objects deleted.

A name longer than a label allows is cut and suffixed with a hash of the full name, as is a Gateway name with dots.

### Ports and privileges

- With a Service, each published port below 1024 binds an unprivileged target port, from 8000 up; other ports bind themselves. The pods drop every capability: restricted Pod Security, OpenShift `restricted-v2`.
- Without a Service, the data plane binds the published ports. A port below 1024 adds `NET_BIND_SERVICE` and nothing else.
- The image carries `cap_net_bind_service` as a file capability (`+p`, not `+ep`): portail raises it when the pod allows it, and the same image still runs where the pod drops it.
- Listener changes never roll the pods: the Deployment declares no listener ports.

### Addresses

- LoadBalancer Service: `status.addresses` is its ingress; an `IPAddress` in `spec.addresses` is not requested from the provider and reports `AddressNotAssigned` unless the provider assigns it anyway.
- ClusterIP or NodePort Service: the cluster IP.
- No Service: what the data planes report: the `IPAddress` values of `spec.addresses` found on a pod, resolved Network addresses, else each pod's first interface IP.
- An address already on a pod interface, or a `portail.epheo.eu/Network` entry, is bound specifically. Other addresses, such as a LoadBalancer VIP, are not bound; with no local address, listeners bind all interfaces.
- A listener that gains a local address leaves all interfaces for it in the same apply, and goes back when it loses it.
- A listener whose published port changes while its bound port stays reopens that socket in the same apply, to route and terminate as the new port.
- A Network entry with no IP on the pod binds nothing, never all interfaces: `Programmed` is `False` with reason `AddressNotUsable`.

## GatewayParameters

Group `portail.epheo.eu`, version `v1alpha1`, namespaced:

```yaml
apiVersion: portail.epheo.eu/v1alpha1
kind: GatewayParameters
metadata:
  name: public
  namespace: portail-system
spec:
  provisioning: Managed      # Managed | Unmanaged
  replicas: 1                # Managed; at least 1
  image: ghcr.io/epheo/portail:X.Y.Z   # Managed; default: the controller's --dataplane-image
  resources: {}              # Managed; a core/v1 ResourceRequirements
  service:
    type: LoadBalancer       # LoadBalancer | ClusterIP | NodePort | None
  networks:                  # Managed; Multus NetworkSelectionElements
  - name: br-ex-network
    namespace: default
    ips: ["172.16.87.21/24"]
    gateway: ["172.16.87.1"]
  controllerTransport: Direct  # Managed; Direct | APIServerProxy, see Primary UDN
```

Referenced from:

- a GatewayClass, `spec.parametersRef` with its `namespace`: defaults for the class;
- a Gateway, `spec.infrastructure.parametersRef`, in the Gateway's namespace: overrides for that Gateway.

Built-in defaults, then the class's, then the Gateway's: a set field wins field by field; `networks` and `resources` replace whole.

Defaults:

- `provisioning: Managed`, `replicas: 1`.
- `resources`: requests `100m`/`64Mi`, limits `2`/`256Mi`. The data plane runs one worker thread per CPU of its limit, at most 4.
- `service.type`: `None` when the Gateway has a Network address or `networks` is set, else `LoadBalancer`.
- `controllerTransport: Direct`.

Rules:

- `networks[].ips` requires `replicas: 1`: one static IP cannot sit on two pods.
- A missing or invalid GatewayParameters makes the Gateway `Accepted=False`, reason `InvalidParameters`; on a class, the class and its Gateways.
- Invalid parameters change nothing that runs: the last configuration and the provisioned objects stay, so a typo never takes a Gateway down.

Multus: the pods get `k8s.v1.cni.cncf.io/networks` with `networks`, then each Network address not already listed.

### Replicas

| | `replicas: 1` | `replicas: 2` |
|---|---|---|
| PodDisruptionBudget | none | `maxUnavailable: 1` |
| Rollout | surge 1, unavailable 0; `Recreate` with static `ips` | surge 1, unavailable 0 |
| Node drain | a gap: the replacement's start, less the shutdown delay | none |
| Controller down, a pod restarts | dark until the controller returns | the other replica serves |

Two replicas spread across nodes when there are several.

## Primary UDN

On OVN-Kubernetes, a pod in a namespace with a Primary User Defined Network reaches the apiserver and DNS on the default network, and nothing else: its data plane cannot dial the controller Service.
GatewayParameters `controllerTransport: APIServerProxy` makes it poll through the apiserver's service proxy:

```
POST https://kubernetes.default.svc/api/v1/namespaces/portail-system/services/https:portail-controller:9443/proxy/v1/sync/<ns>/<name>
```

Use it only for pods that cannot reach the controller Service: Direct is one hop shorter and holds no API token.

RBAC: add `deploy/components/apiserver-proxy` to your kustomization of `deploy/install`, with a RoleBinding of its Role to each such Gateway's ServiceAccount, `portail-<gateway>` in the Gateway's namespace:

```yaml
# kustomization.yaml
apiVersion: kustomize.config.k8s.io/v1beta1
kind: Kustomization
resources:
- https://github.com/epheo/portail//deploy/install?ref=vX.Y.Z
- rolebinding.yaml
components:
- https://github.com/epheo/portail//deploy/components/apiserver-proxy?ref=vX.Y.Z
```

```yaml
# rolebinding.yaml
apiVersion: rbac.authorization.k8s.io/v1
kind: RoleBinding
metadata:
  name: portail-controller-proxy
  namespace: portail-system
roleRef:
  apiGroup: rbac.authorization.k8s.io
  kind: Role
  name: portail-controller-proxy
subjects:          # one per Gateway polling through the apiserver
- kind: ServiceAccount
  name: portail-svc-gw
  namespace: demo-primary
```

- The Role grants `services/proxy` on `https:portail-controller:9443`, verb `create` (a POST): nothing else, and only on that Service port.
- Without the binding the data plane logs `apiserver answered 403 Forbidden ... configuration error`, backs off and keeps polling; nothing it serves changes.

The pod gets:

- `--transport apiserver-proxy`;
- a second projected volume at `/var/run/secrets/portail.epheo.eu/apiserver`: a token of the API audience, one hour, rotated by kubelet, and `kube-root-ca.crt`;
- still `automountServiceAccountToken: false`: the API token is opt-in per Gateway.

The API token is good for whatever the ServiceAccount may do, not only the proxy call: its groups' bindings come with it, `system:serviceaccounts`, `system:serviceaccounts:<namespace>` and `system:authenticated`, and on OpenShift what those are bound to, such as `basic-user` and `system:image-puller`.

An Unmanaged data plane does the same by hand: `--transport apiserver-proxy`, and the two files at the `--apiserver-token-file` and `--apiserver-ca-file` paths ([configuration](configuration.md#portail-dataplane)).

Trust:

- The data plane's TLS ends at the apiserver, verified against `kube-root-ca.crt` as `kubernetes.default.svc`; the apiserver dials the controller without verifying it.
- So the data plane takes an answer only when the controller signed it for that poll (see [Security](#security)): whoever may edit the controller Service or its endpoints can neither forge a configuration nor replay an older one.
- The controller authenticates the data plane as on Direct: a TokenReview of its controller token, carried in `X-Portail-Token` since the apiserver takes `Authorization` for its own hop.
- Trust boundary: whoever can modify the controller Service or its EndpointSlices, and run a pod in the controller namespace, receives the proxied data planes' controller tokens, which the apiserver forwards to the endpoint it picks. That access is admin of the controller namespace, which reads the CA Secret anyway.
- Follow-up: end-to-end TLS through an HTTP Upgrade tunnel across the apiserver would close that.

Scale:

- On OVN-Kubernetes every proxied poll reaches the controller from an apiserver node's management address (`ovn-k8s-mp0`), not from the pod. The controller caps open connections per address with `--sync-peer-connections` (default 32; a data plane holds one or two): set it to at least twice the Primary UDN data planes per apiserver node.
- Reviews of new tokens from one address are paced, two at once and five a second, not refused: after a controller restart such a fleet reconnects over a few seconds.
- Each data plane holds one request open on the apiserver, answered within the 30s hold and sent again.
- The apiserver counts proxy requests as long-running: no request timeout cuts the hold.
- Fine for tens to low hundreds of Gateways; past that, give the pods a route to the controller Service and use Direct.
- With two controller replicas the apiserver picks an endpoint per request; a standby's answer is retried within a second.

See `examples/kubernetes/primary-udn/`.

## Unmanaged Gateways

`provisioning: Unmanaged`: the controller creates and deletes nothing; it serves the configuration and writes status.
The pods are yours: hostNetwork, a DaemonSet, init containers, an SCC.
They must run as ServiceAccount `portail-<gateway>` in the Gateway's namespace, with a projected token of audience `portail-controller.<controller namespace>.svc` and the controller CA.
They bind the published ports: the pods' owner adds `NET_BIND_SERVICE` or other privileges.
Any number of pods may serve the Gateway; each reports, and `Programmed` needs one live pod at the current configuration.

See `examples/kubernetes/unmanaged/`.

## Failure behavior

Controller down:

- Running data planes keep serving their configuration from memory: listeners, routes, certificates, rate limits.
- They keep re-resolving backends: pod churn is still followed.
- Route and certificate changes wait for the controller; `portail_config_age_seconds` grows and `portail_controller_connected` is `0`.
- A data-plane pod that starts meanwhile stays NotReady until the controller answers.

Data plane stopping (SIGTERM):

- It reports leaving at once, keeps serving and `/readyz` at 200 for `--shutdown-delay` (5s), then drains for up to 30s.
- A terminating pod stays a serving endpoint: kube-proxy uses it while no ready one exists.
- With `Recreate` (static IPs) the delay is 0: no other endpoint takes over.
- On OVN-Kubernetes some established connections to a pod are reset as soon as it turns terminating, while it still serves: a rollout of a one-replica Gateway can cost a few requests (measured: 1 in 60 on OpenShift 4.22). Two replicas avoid it for scale changes but not for rollouts.

Data plane gone without a goodbye: it counts as live for 60s after its last report (two holds of 30s); then, with no other live data plane at the current configuration, the Gateway turns `Programmed=False`, reason `Pending`.

Gateway deleted, or its class naming another controller: its data planes close their listeners, turn NotReady and keep polling.
A deleted GatewayClass changes nothing until it returns.

Controller failover: data planes reconnect to the new leader within a retry; an unchanged configuration is not re-applied.

## Security

- Data planes authenticate with projected ServiceAccount tokens of the controller audience; the controller checks them with a TokenReview.
- A Gateway's configuration goes only to its own ServiceAccount, `portail-<gateway>` in its namespace: the trust boundary is the namespace.
- The data-plane pods mount no API token, and the API server refuses the controller-audience one.
- The controller serves TLS from a CA it generates once, kept in Secret `portail-controller-ca` and published in ConfigMap `portail-controller-ca` (key `ca.crt`).
- The sync endpoint and the data planes speak TLS 1.3 only.
- The controller signs every snapshot, "unchanged" and "Gateway gone" answer with its serving key, never the CA's, over a nonce the data plane draws for each poll. A data plane takes an answer only when the signing certificate chains to the controller CA, is a serving certificate for the controller Service, valid now, and the signature covers this poll's nonce and the body ("unchanged": the version it confirms); a "gone" answer must also name its Gateway.
- An answer that fails, or a body over 64 MiB, changes nothing served: the data plane counts the controller as disconnected, backs off, and its reports carry why as an apply error until the controller answers again.
- CA rotation: delete the Secret, restart the controller; every Managed data plane rolls.
- RBAC cannot narrow Secret reads to the TLS Secrets Gateways reference: the controller reads Secrets cluster-wide, and nothing else does.

## Container Image

```
ghcr.io/epheo/portail:X.Y.Z
```

Tags:

- `X.Y.Z` and `X.Y`: releases; `latest` is the last release.
- `main` and `sha-<commit>`: every commit on main, between releases.

A controller renders Managed data planes with the image it was published as: `:X.Y.Z` for a release, `:sha-<commit>` for main.
A local build renders `ghcr.io/epheo/portail:<its version>`; build with `--build-arg PORTAIL_IMAGE=<reference>` to name another.

A release is a `vX.Y.Z` tag on main, made after `scripts/release.sh X.Y.Z` set the versions in a merged commit.

Build from source (compiles inside the build stage):

```bash
podman build -t ghcr.io/epheo/portail:latest -f Containerfile .
```

Or package a host build:

```bash
cargo build --release
podman build --build-arg PREBUILT=1 -t ghcr.io/epheo/portail:latest -f Containerfile .
```

## Probes and Metrics

Both modes serve a plain-HTTP admin endpoint on `--readiness-port` (default `19099`):

- `/livez`: `200` once the process is up. Liveness only: should this process be restarted.
- `/readyz`: the controller once its caches synced and its CA loaded; a data plane once it applied a configuration and every listener of it is bound.
- `/metrics`: Prometheus text format.

Managed data planes start and stay live on `/livez` (startupProbe and livenessProbe) and take traffic on `/readyz`: a pod started while the controller is down stays NotReady until it answers, instead of being restarted in a loop.

Probe the pod IP, never a Gateway VIP: a VIP has no delivery guarantee to this pod and turns a neighbor's outage into your restart loop.
Managed data planes run with `--admin-address=$(POD_IP)`, so a Gateway VIP or Multus interface does not expose `/metrics` (see [configuration](configuration.md#admin-endpoint)).

## Multi-Network Routing

A Gateway with a `portail.epheo.eu/Network` address puts its data plane on that network through Multus and binds the pod's IP there.
GatewayParameters `networks` attach more networks, for the backend side.
Traffic crossing between networks goes through portail, which applies Gateway API routing rules (HTTPRoute, TCPRoute, UDPRoute).

- The controller reads the network's NetworkAttachmentDefinition; the data plane binds its interface IP within that subnet.
- A NetworkAttachmentDefinition that cannot be read keeps its last subnet; one never read keeps the Gateway's previous configuration.

For pure L3 connectivity between subnets (high throughput, ICMP, no L7 policy), use OVN or iptables.
portail complements L3 infrastructure as the L4-L7 policy layer.

See `examples/kubernetes/multi-network/`.

## OpenShift

- `restricted-v2` admits the controller and Managed data planes, including the `NET_BIND_SERVICE` add and the projected token.
- hostNetwork, `NET_ADMIN` init containers and other privileges are for Unmanaged pods, with an SCC the cluster admin grants to their ServiceAccount.

## Verifying

```bash
# GatewayClass accepted, with the features portail supports
kubectl get gatewayclass portail -o jsonpath='{.status.conditions[?(@.type=="Accepted")].status} {.status.supportedFeatures[*].name}{"\n"}'

# Gateway status: Accepted and Programmed True
kubectl get gateway my-gateway -o yaml

# Its data plane
kubectl get deploy,svc,pdb,sa -l portail.epheo.eu/gateway=my-gateway
```

- The controller leader writes every status, as field manager `portail-controller`.
- It writes only the status that differs from what the API server holds, so a wiped or outdated status is rewritten on the next pass.
- A route or RateLimitPolicy that stops naming a Gateway loses portail's status entry for that Gateway.
- A listener a data plane cannot bind reports `Programmed=False`, reason `Pending`, with the error, and the Gateway `Programmed=False`, reason `Invalid`; `Pending` too until a data plane applied the Gateway's current configuration.
- Listeners sharing a port and protocol conflict unless their hostnames differ.
- A UDP listener shares its port with a TCP, HTTP, HTTPS or TLS one; two UDP listeners on one port conflict.
- A Gateway with some listeners rejected stays `Accepted=True` with reason `ListenersNotValid`.
- `attachedRoutes` counts every route attached to the listener, accepted or not.

## Uninstall

Delete the Gateways of class `portail` first: their data planes go with them, and the controller removes its route status entries.

```bash
kubectl get gateways -A -o jsonpath='{range .items[?(@.spec.gatewayClassName=="portail")]}{.metadata.namespace} {.metadata.name}{"\n"}{end}' |
    while read -r ns name; do kubectl -n "$ns" delete gateway "$name"; done
kubectl delete -k "https://github.com/epheo/portail//deploy/install?ref=vX.Y.Z"
```

Deleting the CRDs deletes every GatewayParameters and RateLimitPolicy.
