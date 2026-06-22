# ClusterFuzzLite

Continuous CI fuzzing for Hardy's `cargo-fuzz` targets, via [ClusterFuzzLite](https://google.github.io/clusterfuzzlite/) (the OSS-Fuzz engine hosted in GitHub Actions). Strategy and target inventory live in [`docs/test_strategy.md`](../docs/test_strategy.md) §3.3; this file is the operator runbook.

## Files

- `Dockerfile` — OSS-Fuzz `base-builder-rust`; copies the tree (including `patches/`) to `$SRC/hardy`.
- `build.sh` — builds all five fuzz crates and copies the eight target binaries into `$OUT`. Sets `RUSTUP_TOOLCHAIN=nightly` so the container's nightly is used despite the repo's `stable` pin in `rust-toolchain.toml` (which is left untouched).

Workflows: `.github/workflows/cflite-pr.yml` (per-PR `code-change`, 5 min) and `.github/workflows/cflite-cron.yml` (nightly `batch`, 15 min/target).

## Reproducing a crash

CI emits every crash as a minimised testcase (the raw crashing input). Replay it with the same `cargo-fuzz` flow used locally — nightly is required because the repo pins `stable`:

```bash
cargo +nightly fuzz run --fuzz-dir bpv7/fuzz random_bundles path/to/crash-testcase
```

CI and local both default to AddressSanitizer, so ASan-only bugs (heap overflow, use-after-free) reproduce faithfully. For a fully hermetic repro — identical pinned nightly, sanitizer, and flags as CI, independent of your local toolchain — reproduce inside the build image:

```bash
docker build -t hardy-fuzz -f .clusterfuzzlite/Dockerfile .
docker run --rm -v "$PWD/crash:/crash" hardy-fuzz /out/random_bundles /crash/crash-testcase
```

For ad-hoc local fuzzing of a single target, run cargo-fuzz directly (continuous fuzzing is CFLite's job now):

```bash
cargo +nightly fuzz run --fuzz-dir bpv7/fuzz random_bundles
```

## Corpus persistence

Corpus lives in a sibling repo, [`ricktaylor/hardy-fuzz-corpus`](https://github.com/ricktaylor/hardy-fuzz-corpus). CFLite's git filestore reads/writes each fuzzer's corpus under `corpus/<fuzz_target>/`, and publishes coverage reports to the `gh-pages` branch — both wired up in `cflite-pr.yml` (PR runs start from the accumulated corpus) and `cflite-cron.yml` (batch grows it, prune minimises it, coverage reports on it).

**One secret is required:** add a `FUZZ_CORPUS_ACCESS_TOKEN` repo secret to the hardy repo, holding a token with `contents: write` on `hardy-fuzz-corpus`. Without it the storage-repo URL won't authenticate and the runs can't push.

The repo was seeded once from local fuzzing (the `corpus/<target>/` directories); CFLite's `prune` mode minimises it from there. To re-seed or top up later, copy any `*/fuzz/corpus/<target>/` into `corpus/<target>/` of the corpus repo and push.

Coverage reports (once `gh-pages` is enabled on the corpus repo):
`https://ricktaylor.github.io/hardy-fuzz-corpus/coverage/latest/report/linux/report.html`

## Validating the setup

This config has not yet had a CI run. After the `FUZZ_CORPUS_ACCESS_TOKEN` secret is set, trigger `ClusterFuzzLite Cron` via *Run workflow* (`workflow_dispatch`), then check:

- **Binary discovery in `build.sh`** — the `find … release/<target>` copy step. If cargo-fuzz emits to a per-fuzz-dir target directory rather than the workspace target, the path glob needs narrowing. Most likely thing to need a tweak.
- **`cargo fuzz --fuzz-dir`** enumeration in `build.sh` — the awk parse of `[[bin]]` names is the proven fallback if the flag misbehaves.
- **The coverage build** — `sanitizer: coverage` (`-s none` in `build.sh`) plus the harness's coverage flags.
