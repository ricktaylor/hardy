# Changelog

All notable changes to `hardy-echo-service` are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this crate adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.2.0]

### Changed
- **BREAKING:** raised the `hardy-bpa`/`hardy-bpv7`/`hardy-async` requirements to their incompatible releases. `EchoService` implements `hardy_bpa::services::Service`, so consumers must move to `hardy-bpa` 0.2 in lockstep.
- Build echo replies via the chunked zero-copy Editor output, reusing the inbound buffer when possible to avoid an extra payload copy.

Releases before this version predate this changelog; see the git history for details.
