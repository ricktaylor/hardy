# hardy-localdisk-storage

Local filesystem bundle storage backend for Hardy.

Part of the [Hardy](https://github.com/ricktaylor/hardy) DTN Bundle Protocol implementation.
Implements the `BundleStorage` trait from hardy-bpa, storing bundles as files on disk.
Configurable store directory (platform-aware defaults) and optional fsync for durability.

## Documentation

- [Design](docs/design.md)
- [Test Plan](docs/test_plan.md)

## Licence

Apache 2.0 -- see [LICENSE](../LICENSE)
