# Build portail in your dev environment first:
#   cargo build --release
#
# Then build the container image:
#   podman build -t quay.io/epheo/portail:latest -f Containerfile .

FROM registry.fedoraproject.org/fedora-minimal:43
RUN microdnf install -y libgcc && microdnf clean all
COPY target/release/portail /usr/local/bin/portail
ENTRYPOINT ["/usr/local/bin/portail"]
CMD ["--kubernetes"]
