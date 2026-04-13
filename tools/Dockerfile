# ── Stage 1: chef base ────────────────────────────────────────────────────────
FROM rust:1-slim-bookworm AS chef
RUN apt-get update && apt-get install -y --no-install-recommends \
        build-essential \
        protobuf-compiler \
        libprotobuf-dev \
    && rm -rf /var/lib/apt/lists/* \
    && cargo install cargo-chef
WORKDIR /build

# ── Stage 2: planner ──────────────────────────────────────────────────────────
FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

# ── Stage 3: builder ──────────────────────────────────────────────────────────
FROM chef AS builder
COPY --from=planner /build/recipe.json recipe.json
COPY patches/ patches/
RUN cargo chef cook --release --recipe-path recipe.json -p hardy-tools -p hardy-bpv7-tools -p hardy-cbor-tools
COPY . .
RUN cargo build --release -p hardy-tools -p hardy-bpv7-tools -p hardy-cbor-tools

# ── Stage 4: runtime ──────────────────────────────────────────────────────────
FROM debian:bookworm-slim AS runtime
RUN apt-get update && apt-get install -y --no-install-recommends \
        bash \
        ca-certificates \
        netcat-openbsd \
    && rm -rf /var/lib/apt/lists/*
COPY --from=builder /build/target/release/bp /usr/local/bin/bp
COPY --from=builder /build/target/release/bundle /usr/local/bin/bundle
COPY --from=builder /build/target/release/cbor /usr/local/bin/cbor
