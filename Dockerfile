# syntax=docker/dockerfile:1
FROM docker.io/library/rust:1.98.0-trixie AS builder
WORKDIR /build
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY src ./src
RUN cargo build --locked --release --bin macmapd

# Buildx --output type=local exports just the executable from this stage.
FROM scratch AS binary
COPY --from=builder /build/target/release/macmapd /macmapd

FROM gcr.io/distroless/cc-debian13:nonroot AS runtime
COPY --from=builder /build/target/release/macmapd /usr/local/bin/macmapd
EXPOSE 67/udp 8080/tcp
ENTRYPOINT ["/usr/local/bin/macmapd"]
