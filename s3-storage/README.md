# hardy-s3-storage

Amazon S3 bundle storage backend for Hardy.

Part of the [Hardy](https://github.com/ricktaylor/hardy) DTN Bundle Protocol implementation.
Implements the `BundleStorage` trait from hardy-bpa, storing bundles as S3 objects with
SigV4a authentication and multipart upload for large bundles. Compatible with MinIO and
other S3-compatible stores via configurable endpoint URL and path-style addressing.

## Installation

```toml
[dependencies]
hardy-s3-storage = "0.1"
```

Published on [crates.io](https://crates.io/crates/hardy-s3-storage).

## Documentation

- [Design](docs/design.md)
- [Test Coverage](docs/test_coverage_report.md)
- [User Documentation](https://ricktaylor.github.io/hardy/configuration/storage/#amazon-s3)

## Licence

Apache 2.0 -- see [LICENSE](../LICENSE)
