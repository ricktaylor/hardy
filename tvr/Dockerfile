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
ARG CARGO_ARGS=""
COPY --from=planner /build/recipe.json recipe.json
COPY patches/ patches/
RUN cargo chef cook --release --recipe-path recipe.json -p hardy-tvr $CARGO_ARGS
COPY . .
RUN cargo build --release --bin hardy-tvr $CARGO_ARGS

# ── Stage 4: minimal runtime image ───────────────────────────────────────────
FROM gcr.io/distroless/cc-debian12 AS runtime
COPY --from=builder \
    /build/target/release/hardy-tvr \
    /usr/local/bin/hardy-tvr
EXPOSE 50052
ENTRYPOINT ["/usr/local/bin/hardy-tvr"]
