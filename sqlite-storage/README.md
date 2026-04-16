# hardy-sqlite-storage

SQLite metadata storage backend for Hardy.

Part of the [Hardy](https://github.com/ricktaylor/hardy) DTN Bundle Protocol implementation.
Implements the `MetadataStorage` trait from hardy-bpa, persisting bundle metadata and
forwarding state in a local SQLite database with automatic schema migration.

## Installation

```toml
[dependencies]
hardy-sqlite-storage = "0.5"
```

Published on [crates.io](https://crates.io/crates/hardy-sqlite-storage).

## Documentation

- [Design](docs/design.md)
- [Test Coverage](docs/test_coverage_report.md)
- [User Documentation](https://ricktaylor.github.io/hardy/configuration/storage/#sqlite)

## Licence

Apache 2.0 -- see [LICENSE](../LICENSE)
