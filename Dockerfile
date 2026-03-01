# ── Stage 1: build ────────────────────────────────────────────────────────────
FROM rust:1-slim-bookworm AS builder
RUN apt-get update && apt-get install -y --no-install-recommends \
        build-essential \
        protobuf-compiler \
        libprotobuf-dev \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /build
COPY . .
RUN cargo build --release --bin hardy-bpa-server

# ── Stage 2: minimal runtime image ───────────────────────────────────────────
FROM gcr.io/distroless/cc-debian12 AS runtime
COPY --from=builder \
    /build/target/release/hardy-bpa-server \
    /usr/local/bin/hardy-bpa-server
EXPOSE 50051
ENTRYPOINT ["/usr/local/bin/hardy-bpa-server"]
