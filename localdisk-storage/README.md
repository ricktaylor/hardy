# hardy-localdisk-storage

Local filesystem bundle storage backend for Hardy.

Part of the [Hardy](https://github.com/ricktaylor/hardy) DTN Bundle Protocol implementation.
Implements the `BundleStorage` trait from hardy-bpa, storing bundles as files on disk.
Configurable store directory (platform-aware defaults) and optional fsync for durability.

## Installation

```toml
[dependencies]
hardy-localdisk-storage = "0.5"
```

Published on [crates.io](https://crates.io/crates/hardy-localdisk-storage).

## Documentation

- [Design](docs/design.md)
- [Test Coverage](docs/test_coverage_report.md)
- [User Documentation](https://ricktaylor.github.io/hardy/configuration/storage/#local-disk)

## Licence

Apache 2.0 -- see [LICENSE](../LICENSE)
