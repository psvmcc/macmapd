# syntax=docker/dockerfile:1
FROM docker.io/library/rust:1.98.0-trixie AS builder
WORKDIR /build
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY src ./src
RUN cargo build --locked --release --bin macdack

# Buildx --output type=local exports just the executable from this stage.
FROM scratch AS binary
COPY --from=builder /build/target/release/macdack /macdack

FROM gcr.io/distroless/cc-debian13:nonroot AS runtime
LABEL org.opencontainers.image.title="macdack" \
      org.opencontainers.image.source="https://github.com/psvmcc/macdack" \
      org.opencontainers.image.licenses="MIT"
COPY --from=builder /build/target/release/macdack /usr/local/bin/macdack
EXPOSE 67/udp 8080/tcp
ENTRYPOINT ["/usr/local/bin/macdack"]
