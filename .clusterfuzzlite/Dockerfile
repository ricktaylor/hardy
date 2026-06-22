# ClusterFuzzLite / OSS-Fuzz build image for Hardy's cargo-fuzz targets.
# base-builder-rust ships a pinned nightly Rust, cargo-fuzz, and the sanitizer
# runtimes ClusterFuzzLite expects.
FROM gcr.io/oss-fuzz-base/base-builder-rust

# OSS-Fuzz convention: project sources live under $SRC/<project>, the build
# script at $SRC/build.sh. Copying the whole tree brings patches/ along, which
# the workspace's [patch.crates-io] = patches/sqlx-sqlite needs to resolve.
COPY . $SRC/hardy
WORKDIR $SRC/hardy
COPY .clusterfuzzlite/build.sh $SRC/
