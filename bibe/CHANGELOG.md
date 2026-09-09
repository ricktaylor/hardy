# Changelog

All notable changes to `hardy-bibe` are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this crate adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Changed
- An encapsulated outer bundle that exceeds the size cap negotiated at registration is rejected deterministically at `forward` with `PayloadTooLarge` (a per-bundle error, so the peer queue is not swept), instead of being dispatched into a certain rejection. When no cap was negotiated, no pre-check applies.
