# hardy-postgres-storage

PostgreSQL metadata storage backend for Hardy.

Part of the [Hardy](https://github.com/ricktaylor/hardy) DTN Bundle Protocol implementation.
Implements the `MetadataStorage` trait from hardy-bpa, persisting bundle metadata in
PostgreSQL with connection pooling and automatic schema migration via sqlx.

## Installation

```toml
[dependencies]
hardy-postgres-storage = "0.1"
```

Published on [crates.io](https://crates.io/crates/hardy-postgres-storage).

## Documentation

- [Design](docs/design.md)
- [Test Coverage](docs/test_coverage_report.md)
- [User Documentation](https://ricktaylor.github.io/hardy/configuration/storage/#postgresql)

## Licence

Apache 2.0 -- see [LICENSE](../LICENSE)
