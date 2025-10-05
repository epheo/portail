# UringRess

This project is a way for me to learn about io_uring and eBPF.

For now, this is just a TCP/HTTP loadbalancer that you can configure with a yaml file.

In the future this should be a Kubernetes Gateway API Controller, hopefully offering decent performances.

Protocol detection and worker selection is performed by the eBPF program,
the io_uring workers are then handling the traffic.
Each worker is usually configured with a speciality (http, tcp etc)

For now packets go from kernel to user space and back to kernel, this could be optimized with AF_XDP and bypassing kernel network stack, but I'll try to deal with the current (already too high) complexity before introducing more.

## License

Apache-2.0
