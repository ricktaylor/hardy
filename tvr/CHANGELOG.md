# Changelog

All notable changes to `hardy-tvr` are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Changed
- **BREAKING**: the process ends when its routing session ends. A session lost to a dropped connection, or closed by the BPA (its shutdown or restart), exits nonzero so a `Restart=on-failure` supervisor restarts the agent; previously the process kept running with a dead sink and only ever exited 0. SIGINT/SIGTERM still exit 0. The explicit in-band unregister on shutdown is gone: the session's teardown is the unregistration, and the BPA withdraws the agent's routes on it either way.

## [0.2.0]

### Changed
- Use the shared `hardy-async` file watcher and decouple the watch config from the runtime watch mode.
- Track the `hardy-bpa` routing module restructure (dedicated routing table + fine-grained route actions).
- Raised all internal `hardy-*` dependency requirements to the v0.2.0 release line.
- Raised the minimum supported Rust version (MSRV) to 1.95.

Releases before this version predate this changelog; see the git history for details.
