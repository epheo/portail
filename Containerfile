# Multi-stage build: compile Rust binary, then copy into minimal runtime image.
#
# Build locally:
#   podman build -t ghcr.io/epheo/portail:latest -f Containerfile .
#
# Or with a pre-built binary (faster iteration):
#   cargo build --release
#   podman build --target runtime --build-arg PREBUILT=1 -t ghcr.io/epheo/portail:latest -f Containerfile .

FROM docker.io/library/rust:1-slim AS builder
RUN apt-get update && apt-get install -y pkg-config libssl-dev && rm -rf /var/lib/apt/lists/*
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src/ src/
COPY benches/ benches/
RUN cargo build --release --bin portail && strip target/release/portail

FROM registry.fedoraproject.org/fedora-minimal:43 AS runtime
RUN microdnf install -y libgcc libcap && microdnf clean all
COPY --from=builder /src/target/release/portail /usr/local/bin/portail
# Carry CAP_NET_BIND_SERVICE as a *file* capability so the data plane can bind
# privileged ports (80/443) directly in multi-network/UDN and host-network modes
# while staying non-root under restricted Pod Security (no allowPrivilegeEscalation,
# no custom SCC). Use `+p` (permitted, NOT `+ep`): a binary with the effective bit
# fails to exec when the capability isn't in the bounding set — exactly the
# service-mode pod that drops it. portail raises it to effective at startup only
# when present (src/privileges.rs). `setcap` needs libcap + CAP_SETFCAP at build.
RUN setcap cap_net_bind_service=+p /usr/local/bin/portail
USER 1000
ENTRYPOINT ["/usr/local/bin/portail"]
CMD ["--kubernetes"]
