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
# The workspace root Cargo.toml has a [patch.crates-io] pointing to patches/sqlx-sqlite
# (a local stub to resolve a links="sqlite3" conflict). It must be present before
# cargo chef cook so the resolver can find it.
COPY patches/ patches/
RUN cargo chef cook --release --recipe-path recipe.json $CARGO_ARGS
COPY . .
RUN cargo build --release --bin hardy-bpa-server $CARGO_ARGS

# ── Stage 4: minimal runtime image ───────────────────────────────────────────
FROM gcr.io/distroless/cc-debian12 AS runtime
COPY --from=ghcr.io/grpc-ecosystem/grpc-health-probe:v0.4.38 /ko-app/grpc-health-probe /bin/grpc_health_probe
COPY --from=builder \
    /build/target/release/hardy-bpa-server \
    /usr/local/bin/hardy-bpa-server
EXPOSE 50051
ENTRYPOINT ["/usr/local/bin/hardy-bpa-server"]
