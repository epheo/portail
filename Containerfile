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
RUN microdnf install -y libgcc && microdnf clean all
COPY --from=builder /src/target/release/portail /usr/local/bin/portail
ENTRYPOINT ["/usr/local/bin/portail"]
CMD ["--kubernetes"]
