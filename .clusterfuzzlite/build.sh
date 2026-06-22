#!/bin/bash -eu
# ClusterFuzzLite / OSS-Fuzz build script for Hardy.
# Runs inside gcr.io/oss-fuzz-base/base-builder-rust with $SRC, $OUT and
# $SANITIZER set by the harness.

# Hardy pins channel = "stable" in rust-toolchain.toml; cargo-fuzz needs nightly.
# RUSTUP_TOOLCHAIN outranks rust-toolchain.toml in rustup's precedence, so this
# selects the image's nightly for the fuzz build only — the repo file (and the
# stable pin every other contributor relies on) is left untouched.
export RUSTUP_TOOLCHAIN=nightly

# Persist cargo's registry and build artifacts under $WORK, which OSS-Fuzz
# bind-mounts from the host, so a CI cache can preserve them across runs. The
# default in-container target dir lives in the ephemeral image layer and is
# discarded every build, forcing a full from-scratch compile that dominates CI
# wall-time. A per-sanitizer target dir keeps the address and coverage builds
# from invalidating each other's incremental state.
export CARGO_HOME="${WORK:-/work}/cargo"
export CARGO_TARGET_DIR="${WORK:-/work}/target/${SANITIZER:-address}"
mkdir -p "$CARGO_HOME" "$CARGO_TARGET_DIR"

cd "$SRC/hardy"

# cargo-fuzz defaults to AddressSanitizer. For OSS-Fuzz coverage runs the
# harness sets SANITIZER=coverage and supplies its own instrumentation flags,
# so disable cargo-fuzz's sanitizer in that case.
sanitizer_flag="address"
if [ "${SANITIZER:-address}" = "coverage" ]; then
    sanitizer_flag="none"
fi

# Discover fuzz crates and their targets from the repo itself: the [[bin]]
# entries in each */fuzz/Cargo.toml are the single source of truth, so there is
# no hand-maintained list to drift out of sync (run_lcov.sh derives the same way).
for fuzz_dir in */fuzz; do
    [ -d "$fuzz_dir/fuzz_targets" ] || continue

    echo "=== building $fuzz_dir (sanitizer=$sanitizer_flag) ==="
    cargo fuzz build -O -s "$sanitizer_flag" --fuzz-dir "$fuzz_dir"

    # Place each target binary into $OUT. cargo-fuzz emits under the host triple
    # in the target dir; search $CARGO_TARGET_DIR first, but fall back to the
    # tree in case cargo-fuzz ignores CARGO_TARGET_DIR, so relocating the cache
    # can never break the binary copy.
    while IFS= read -r target; do
        [ -n "$target" ] || continue

        bin="$(find "$CARGO_TARGET_DIR" "$SRC/hardy" -type f \
            -path "*x86_64-unknown-linux-gnu/release/$target" ! -name '*.d' 2>/dev/null | head -n1)"
        if [ -z "$bin" ]; then
            echo "ERROR: built binary for '$target' not found" >&2
            exit 1
        fi
        cp "$bin" "$OUT/$target"

        # Seed corpus is git-ignored, so it is normally absent in a fresh
        # checkout. If one happens to be present, ship it as a seed corpus zip.
        corpus="$fuzz_dir/corpus/$target"
        if command -v zip >/dev/null 2>&1 && \
           [ -d "$corpus" ] && [ -n "$(ls -A "$corpus" 2>/dev/null)" ]; then
            (cd "$corpus" && zip -q -r "$OUT/${target}_seed_corpus.zip" .)
        fi
    done < <(awk -F'"' '/^\[\[bin\]\]/{b=1;next} b&&/name *=/{print $2; b=0}' "$fuzz_dir/Cargo.toml")
done

# TEMPORARY (cache wiring): report where cargo wrote and how large the caches
# are, so the workflow can target the right host path. Remove once the
# actions/cache step is confirmed working.
echo "=== cargo cache footprint (in-container WORK=$WORK) ==="
du -sh "$CARGO_HOME" "$CARGO_TARGET_DIR" 2>/dev/null || true
