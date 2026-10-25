# Upgrading

## 0.1.x to 0.2.0

0.2.0 splits portail into a controller and one data plane per Gateway.
There is no compatibility path: install the controller, remove the old data planes and their RBAC, then recreate or reference the Gateways.
Plan a short outage per Gateway.

### What changed

- `--kubernetes`, `--gateway`, `--watch-shape` and `--manage-gateway-status` are gone: `portail controller` and `portail dataplane` replace them.
- Standalone mode (`portail --config FILE`) is unchanged, but for the duration parser below.
- The controller provisions each Gateway's data plane: portail-operator is not used any more.
- Data-plane pods have no RBAC and no API token: only the controller talks to the API server and reads Secrets.
- The controller writes every status, as field manager `portail-controller`, GatewayClass status included (`Accepted`, `supportedFeatures`).
- New CRD GatewayParameters: replicas, image, resources, Service type, Multus networks, and `provisioning: Unmanaged` for pods you write yourself.
- The RateLimitPolicy CRD moved from `examples/kubernetes/crds/` to `deploy/crds/`; `deploy/install` installs both CRDs.
- `examples/kubernetes/base/`, the DaemonSet example and the hand-written Deployments are gone: `deploy/install` installs the controller; see `examples/kubernetes/` for Gateways and GatewayParameters.
- `--http2` and `--access-log` are data-plane flags: Managed data planes do not set them yet, Unmanaged ones can.

### Durations

Config files and Gateway API resources share one stricter duration parser:

- `+5s`, `1e3s`, `infs` and `nanms` no longer load.
- Durations of several units, such as an HTTPRoute timeout of `1m30s`, now apply; 0.1.x ignored them.

Check a config file before the upgrade: `portail --config FILE --validate-only`.

### From the plain manifests (DaemonSet or hand-written Deployments)

1. Install the controller. `--force-conflicts` takes over the ServiceAccount, ClusterRole and ClusterRoleBinding `portail-controller`, and the GatewayClass `portail`, which keep their names:

   ```bash
   kubectl apply --server-side --force-conflicts -k "https://github.com/epheo/portail//deploy/install?ref=v0.2.0"
   kubectl -n portail-system rollout status deploy/portail-controller
   ```

2. Delete the old data planes, their Services and, on OpenShift, their SCC. The old examples named them all `portail`:

   ```bash
   kubectl -n portail-system delete daemonset/portail deployment/portail service/portail --ignore-not-found
   kubectl delete scc portail --ignore-not-found
   ```

3. For each Gateway, choose:
   - Managed, the default: the controller creates `portail-<gateway>` (Deployment, Service, ServiceAccount) in the Gateway's namespace. A LoadBalancer Service gets a new address.
   - Unmanaged: keep your own pods, turned into data planes. See below.

4. Clear status written by 0.1.x, once per Gateway: fields only the old `portail` field manager owned would otherwise outlive a later listener removal. The controller rewrites status within a pass.

   ```bash
   kubectl -n <namespace> patch gateway <name> --subresource=status --type=merge -p '{"status":null}'
   ```

### From portail-operator

The controller does not adopt the operator's objects: delete the Gateways, then recreate them.

1. Save the Gateways without their status:

   ```bash
   kubectl get gateways -A -o yaml > gateways.yaml
   ```

   Strip `status`, `metadata.uid`, `metadata.resourceVersion`, `metadata.creationTimestamp` and `metadata.managedFields` from each item.

2. Uninstall the operator (its Subscription and CSV under OLM, or `kubectl delete -k config/default` from its repository) and its console plugin.

3. Delete the Gateways: garbage collection removes their `portail-<gateway>` Deployments, Services and PDBs.
   A delete that hangs on a finalizer: `kubectl -n <namespace> patch gateway <name> --type=merge -p '{"metadata":{"finalizers":null}}'`.

4. Delete what is left of the operator: the `portail-controller` ServiceAccounts it created in Gateway namespaces, its `portail-operator-*` ClusterRoles and ClusterRoleBindings, and its namespace.
   Strip the finalizer it put on the GatewayClass; 0.2.0 manages none:

   ```bash
   kubectl patch gatewayclass portail --type=merge -p '{"metadata":{"finalizers":null}}'
   ```

5. Install the controller as above, with `--force-conflicts` for the GatewayClass fields the operator owned.

6. Recreate the Gateways. The operator's `--default-replicas` becomes GatewayParameters `replicas`, on the class or per Gateway: 0.2.0 defaults to 1 replica, the operator to 2.

### Hand-written pods as Unmanaged data planes

For pods the controller will not render: hostNetwork, `NET_ADMIN` init containers, static Multus IPs with a custom default route.

1. A GatewayParameters with `provisioning: Unmanaged`, referenced by the GatewayClass `spec.parametersRef` or by each Gateway `spec.infrastructure.parametersRef`.
2. A ServiceAccount `portail-<gateway>` in the Gateway's namespace, `automountServiceAccountToken: false`; an SCC names it instead of the old one.
3. In the pod template:
   - `serviceAccountName: portail-<gateway>`, `automountServiceAccountToken: false`, `terminationGracePeriodSeconds: 40`;
   - args `dataplane --gateway <namespace>/<name> --controller-address https://portail-controller.portail-system.svc:9443 --controller-ca-file /etc/portail/controller-ca/ca.crt`;
   - `--shutdown-delay 0s` for a single pod on a static IP, where no other endpoint takes over;
   - a projected token of audience `portail-controller.portail-system.svc` at `/var/run/secrets/portail.epheo.eu/controller`;
   - ConfigMap `portail-controller-ca` at `/etc/portail/controller-ca`, copied into the Gateway's namespace when it is not `portail-system`;
   - no TLS Secret volume: certificates come from the controller.
4. The same image version as the controller: data planes upgrade with it.

`examples/kubernetes/unmanaged/` is a complete example.

### Rolling back

Keep the 0.1.x manifests and image.

1. Scale the controller to zero: `kubectl -n portail-system scale deploy/portail-controller --replicas=0`.
2. Re-apply the 0.1.x objects; `kubectl replace` restores saved ones exactly.
3. Delete the data planes the controller created: `kubectl delete deploy,svc,pdb,sa -A -l app.kubernetes.io/managed-by=portail-controller`.

GatewayParameters and the GatewayClass `parametersRef` are ignored by 0.1.x.
