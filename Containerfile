FROM rust:latest AS builder
WORKDIR /build
COPY . .
RUN cargo build --release

FROM scratch
COPY --from=builder /build/target/release/uringress /uringress
ENTRYPOINT ["/uringress"]
CMD ["--kubernetes"]
