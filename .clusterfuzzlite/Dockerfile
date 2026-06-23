# ClusterFuzzLite / OSS-Fuzz build image for Hardy's cargo-fuzz targets.
# base-builder-rust ships a pinned nightly Rust, cargo-fuzz, and the sanitizer
# runtimes ClusterFuzzLite expects.
FROM gcr.io/oss-fuzz-base/base-builder-rust

# The coverage build compiles the whole workspace — including hardy-proto, whose
# build.rs runs prost-build — so protoc must be present (the address fuzz build
# doesn't reach hardy-proto, but the coverage build does). The OSS-Fuzz base
# image (Ubuntu 20.04) only ships protoc 3.6 via apt, which is too old for
# prost-build's --experimental_allow_proto3_optional flag (needs protobuf
# >= 3.12), so install a current release directly instead.
ARG PROTOC_VERSION=28.3
RUN apt-get update && \
    apt-get install -y --no-install-recommends ca-certificates curl unzip && \
    curl -fsSL -o /tmp/protoc.zip \
      "https://github.com/protocolbuffers/protobuf/releases/download/v${PROTOC_VERSION}/protoc-${PROTOC_VERSION}-linux-x86_64.zip" && \
    unzip -q /tmp/protoc.zip -d /usr/local && \
    rm /tmp/protoc.zip && \
    rm -rf /var/lib/apt/lists/*
ENV PROTOC=/usr/local/bin/protoc

# OSS-Fuzz convention: project sources live under $SRC/<project>, the build
# script at $SRC/build.sh. Copying the whole tree brings patches/ along, which
# the workspace's [patch.crates-io] = patches/sqlx-sqlite needs to resolve.
COPY . $SRC/hardy
WORKDIR $SRC/hardy
COPY .clusterfuzzlite/build.sh $SRC/
