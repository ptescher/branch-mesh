# syntax=docker/dockerfile:1
#
# BranchMesh: single static-ish binary (API + embedded web UI) on distroless.
#   docker build -t branchmesh .
#   docker run -p 8080:8080 -v branchmesh-data:/data -e ANTHROPIC_API_KEY=... branchmesh

FROM rust:1-slim-bookworm AS builder
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked -p branchmesh-server \
 && cp target/release/branchmesh /usr/local/bin/branchmesh \
 && mkdir -p /data-dir

FROM gcr.io/distroless/cc-debian12:nonroot
COPY --from=builder /usr/local/bin/branchmesh /usr/local/bin/branchmesh
COPY --from=builder --chown=65532:65532 /data-dir /data
ENV BRANCHMESH_DATA=/data/branchmesh.db \
    BRANCHMESH_BIND=0.0.0.0:8080 \
    RUST_LOG=info
VOLUME ["/data"]
EXPOSE 8080
HEALTHCHECK --interval=30s --timeout=5s --start-period=5s --retries=3 \
  CMD ["/usr/local/bin/branchmesh", "healthcheck"]
ENTRYPOINT ["/usr/local/bin/branchmesh"]
CMD ["serve"]
