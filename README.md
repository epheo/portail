# portail

**portail** is a Kubernetes API Gateway using Tokio runtime.

This project was originaly a way for me to learn about io_uring and eBPF.
But in front of the complexity, I took a step back and removed the io_uring
implementation that was getting out of hands and used Tokio instead.

It is a single person project, obviously **not production ready** in any way.
Many parts have been written by Claude Code LLM.

You have been warned.

That being said, performances are decent and I'm using it for my Microshift homelab.

## License

Apache-2.0
