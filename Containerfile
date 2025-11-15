FROM rust:latest AS builder
WORKDIR /build
COPY . .
RUN cargo build --release

FROM scratch
COPY --from=builder /build/target/release/portail /portail
ENTRYPOINT ["/portail"]
CMD ["--kubernetes"]
