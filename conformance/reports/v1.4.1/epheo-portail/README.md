# Portail

Portail is a Kubernetes Gateway API controller written in Rust.

## Table of Contents

| API channel  | Implementation version                                            | Mode    | Report                                                            |
|--------------|-------------------------------------------------------------------|---------|-------------------------------------------------------------------|
| experimental | [v0.1.0](https://github.com/epheo/portail/releases/tag/v0.1.0)   | default | [v0.1.0 report](./experimental-v0.1.0-default-report.yaml)       |

## Reproduce

Conformance tests can be reproduced from the [Portail repository](https://github.com/epheo/portail):

```bash
git clone https://github.com/epheo/portail.git
cd portail/conformance
go test -v -timeout 20m -run TestConformance -count=1
```

This creates an ephemeral Kind cluster with MetalLB, builds and deploys Portail,
installs the experimental Gateway API CRDs (v1.4.1), and runs the full conformance suite.

The CI workflow is available at
[`.github/workflows/conformance.yml`](https://github.com/epheo/portail/blob/main/.github/workflows/conformance.yml).
