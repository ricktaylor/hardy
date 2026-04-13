# hardy-postgres-storage

PostgreSQL metadata storage backend for Hardy.

Part of the [Hardy](https://github.com/ricktaylor/hardy) DTN Bundle Protocol implementation.
Implements the `MetadataStorage` trait from hardy-bpa, persisting bundle metadata in
PostgreSQL with connection pooling and automatic schema migration via sqlx.

## Documentation

- [Design](docs/design.md)

## Licence

Apache 2.0 -- see [LICENSE](../LICENSE)
