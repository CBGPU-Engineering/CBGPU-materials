# Authenticated Rust file transfer service.
FROM rust:1.88-bookworm AS build
WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --release --locked --jobs 4

FROM debian:bookworm-slim
RUN mkdir -p /data && chown 65532:65532 /data
COPY --from=build /build/target/release/cbgpu-materials /usr/local/bin/cbgpu-materials
USER 65532:65532
ENV CBGPU_MATERIALS_DIR=/data \
    CBGPU_MATERIALS_LISTEN=0.0.0.0:8421 \
    CBGPU_MATERIALS_CONTAINER_BIND=1 \
    CBGPU_MATERIALS_WRITES_ENABLED=0
EXPOSE 8421
HEALTHCHECK --interval=30s --timeout=8s --start-period=20s --retries=3 \
  CMD ["/usr/local/bin/cbgpu-materials", "healthcheck"]
ENTRYPOINT ["/usr/local/bin/cbgpu-materials"]
