# Multi-stage build: compile Rust binary, then copy into minimal runtime image.
#
# Build locally:
#   podman build -t ghcr.io/epheo/portail:latest -f Containerfile .
#
# Or with a pre-built binary (faster iteration):
#   cargo build --release
#   podman build --build-arg PREBUILT=1 -t ghcr.io/epheo/portail:latest -f Containerfile .

ARG PREBUILT=0

# Link against the runtime's glibc (2.39): a newer build glibc can emit symbol
# versions UBI 10 lacks. The toolchain comes from the official image; Rust's
# dist binaries only need glibc 2.17.
FROM registry.access.redhat.com/ubi10/ubi:latest AS builder
RUN dnf install -y gcc && dnf clean all
COPY --from=docker.io/library/rust:1-slim /usr/local/rustup /usr/local/rustup
COPY --from=docker.io/library/rust:1-slim /usr/local/cargo /usr/local/cargo
ENV RUSTUP_HOME=/usr/local/rustup CARGO_HOME=/usr/local/cargo PATH=/usr/local/cargo/bin:$PATH
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src/ src/
COPY benches/ benches/
# Compile-time dependency: config/examples.rs embeds these via include_str!
COPY examples/standalone/ examples/standalone/
# The reference this image is published under (publish.yml sets it): the
# controller's default data-plane image. Unset, the binary falls back to
# its version's release tag.
ARG PORTAIL_IMAGE=
RUN PORTAIL_IMAGE="${PORTAIL_IMAGE}" cargo build --release --locked --bin portail && strip target/release/portail

# Only the stage PREBUILT selects gets built, so the default build needs no host binary.
FROM builder AS binary-0
FROM scratch AS binary-1
COPY target/release/portail /src/target/release/portail
FROM binary-${PREBUILT} AS binary

FROM registry.access.redhat.com/ubi10/ubi-minimal:latest AS runtime
RUN microdnf install -y libgcc libcap && microdnf clean all
COPY --from=binary /src/target/release/portail /usr/local/bin/portail
# A PREBUILT host binary can need a newer glibc: fail the build, not the pod.
RUN /usr/local/bin/portail --version >/dev/null
# Carry CAP_NET_BIND_SERVICE as a *file* capability so the data plane can bind
# privileged ports (80/443) directly in multi-network/UDN and host-network modes
# while staying non-root under restricted Pod Security (no allowPrivilegeEscalation,
# no custom SCC). Use `+p` (permitted, NOT `+ep`): a binary with the effective bit
# fails to exec when the capability isn't in the bounding set — exactly the
# service-mode pod that drops it. portail raises it to effective at startup only
# when present (src/privileges.rs). `setcap` needs libcap + CAP_SETFCAP at build.
RUN setcap cap_net_bind_service=+p /usr/local/bin/portail
USER 1000
# No default CMD: manifests always pass a mode; with no arguments the binary
# runs standalone on its built-in default config.
ENTRYPOINT ["/usr/local/bin/portail"]
