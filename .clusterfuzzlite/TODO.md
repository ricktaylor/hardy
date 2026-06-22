# Fuzz CI (ClusterFuzzLite) — remaining steps

Status as of 2026-06-19. Corpus is already pushed + verified; everything below is what's left to make the first run work.

## Done
- [x] `.clusterfuzzlite/` build (Dockerfile + build.sh, derives the 8 targets from `*/fuzz/Cargo.toml`)
- [x] Workflows: `cflite-pr.yml` (code-change) + `cflite-cron.yml` (Batch + Prune + Coverage), wired to `ricktaylor/hardy-fuzz-corpus`
- [x] `run_fuzz.sh` retired; `run_lcov.sh` derives targets + honest "no corpus" skip
- [x] Corpus seeded to `hardy-fuzz-corpus` `main` in `corpus/<target>/` (public repo, commit 8af4aa14, verified)
- [x] Docs: `test_strategy.md` + `test_coverage_report.md` (11→8), `.clusterfuzzlite/README.md`
- [x] Confirmed no fuzz target needs `protoc` (only `proto`/`tvr` invoke it, neither reachable from a fuzz crate; `opentelemetry-proto` uses runtime `prost`/`tonic` only) — dropped `protobuf-compiler` from the Dockerfile

## To do (Rick)
- [ ] Commit + push this `feat/fuzz-ci` branch (signed)
- [ ] Add `FUZZ_CORPUS_ACCESS_TOKEN` secret to `ricktaylor/hardy` — token with `contents: write` on `hardy-fuzz-corpus` (without it, storage-repo push can't authenticate)
- [ ] Enable GitHub Pages (`gh-pages` branch) on `hardy-fuzz-corpus` for coverage reports (free — repo is public)
- [ ] Trigger **ClusterFuzzLite Cron** → *Run workflow* to validate the first run

## Verify on first run (most-likely tweaks)
- [ ] `build.sh` binary copy: `find … x86_64-unknown-linux-gnu/release/<target>` — if cargo-fuzz emits elsewhere, narrow the glob
- [ ] `cargo fuzz build --fuzz-dir` behaviour (awk parse of `[[bin]]` is the fallback)
- [ ] Coverage build (`sanitizer: coverage` / `-s none`)

## Follow-on (later)
- [ ] `cargo llvm-cov` CI job for unit coverage → then retire `run_lcov.sh`

Coverage report URL (once gh-pages is on):
`https://ricktaylor.github.io/hardy-fuzz-corpus/coverage/latest/report/linux/report.html`
