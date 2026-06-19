#!/bin/bash -eu
# ClusterFuzzLite / OSS-Fuzz build script for Hardy.
# Runs inside gcr.io/oss-fuzz-base/base-builder-rust with $SRC, $OUT and
# $SANITIZER set by the harness.

# Hardy pins channel = "stable" in rust-toolchain.toml; cargo-fuzz needs nightly.
# RUSTUP_TOOLCHAIN outranks rust-toolchain.toml in rustup's precedence, so this
# selects the image's nightly for the fuzz build only — the repo file (and the
# stable pin every other contributor relies on) is left untouched.
export RUSTUP_TOOLCHAIN=nightly

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
    # in the workspace target dir; locate it rather than hard-coding the path
    # (verify this resolves on the first CI run — most likely thing to tweak).
    while IFS= read -r target; do
        [ -n "$target" ] || continue

        bin="$(find "$SRC/hardy" -type f \
            -path "*x86_64-unknown-linux-gnu/release/$target" ! -name '*.d' | head -n1)"
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
