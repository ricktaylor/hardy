# ClusterFuzzLite / OSS-Fuzz build image for Hardy's cargo-fuzz targets.
# base-builder-rust ships a pinned nightly Rust, cargo-fuzz, and the sanitizer
# runtimes ClusterFuzzLite expects.
FROM gcr.io/oss-fuzz-base/base-builder-rust

# The coverage build compiles the whole workspace — including hardy-proto, whose
# build.rs runs prost-build — so protoc must be present. The address fuzz build
# alone doesn't reach hardy-proto, but the coverage report build does, so this
# cannot be dropped even though no fuzz target depends on the proto crate.
RUN apt-get update && \
    apt-get install -y --no-install-recommends protobuf-compiler && \
    rm -rf /var/lib/apt/lists/*

# OSS-Fuzz convention: project sources live under $SRC/<project>, the build
# script at $SRC/build.sh. Copying the whole tree brings patches/ along, which
# the workspace's [patch.crates-io] = patches/sqlx-sqlite needs to resolve.
COPY . $SRC/hardy
WORKDIR $SRC/hardy
COPY .clusterfuzzlite/build.sh $SRC/
