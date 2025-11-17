FROM rust:latest AS builder
WORKDIR /build
COPY . .
RUN cargo build --release

FROM alpine:latest
RUN apk add --no-cache libgcc && \
    adduser -D -u 1000 portail
COPY --from=builder /build/target/release/portail /usr/local/bin/portail
USER portail
ENTRYPOINT ["/usr/local/bin/portail"]
CMD ["--kubernetes"]
