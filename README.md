# portail

**portail** is a Kubernetes API Gateway using Tokio runtime and kube-rs.

Both control and data plane are implemented as a single Rust binary.

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
I plan on adding HTTP/2 (and gRPC) support in a future release.

## License

Apache-2.0
