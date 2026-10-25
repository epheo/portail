# portail

**portail** is a Kubernetes API Gateway using Tokio runtime and kube-rs.

Both control and data plane are implemented as a single Rust binary:
`portail controller` watches the cluster and provisions one data plane per
Gateway, `portail dataplane` serves it without any Kubernetes access, and
plain `portail --config FILE` runs standalone.

It is a single person project, obviously **not production ready** in any way and
many parts have been written or refactored by an LLM.

You have been warned.

That being said, performances are quite decent, it does pass official Gateway API 
conformance tests and I'm using it for both my MicroShift homelab and production 
cluster.

This project was originaly a way for me to learn about io_uring and eBPF.
But in front of the complexity, I took a step back and removed both the io_uring
and eBPF implementations that were getting out of hands in favor of Tokio runtime.

Portail targets **Gateway API v1.4.1** and does not support the mesh (GAMMA) profile.
HTTP/2 is opt-in on HTTPS listeners via `--http2`, standalone or in an Unmanaged data plane.
I plan on adding gRPC support in a future release.

## Install

With the Gateway API CRDs installed, at a release tag:

```bash
kubectl apply --server-side -k "https://github.com/epheo/portail//deploy/install?ref=vX.Y.Z"
```

At a commit of main, `deploy/install` still names the last release's image: set the commit's own in a kustomization of your own.

```yaml
resources:
- https://github.com/epheo/portail//deploy/install?ref=<full commit sha>
images:
- name: ghcr.io/epheo/portail
  newTag: sha-<full commit sha>
```

Then create Gateways of class `portail`. See [deployment](docs/deployment.md),
[configuration](docs/configuration.md), [upgrading from 0.1.x](docs/upgrading.md)
and [examples](examples/kubernetes/).

## License

Apache-2.0
