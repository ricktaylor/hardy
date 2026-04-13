# hardy-s3-storage

Amazon S3 bundle storage backend for Hardy.

Part of the [Hardy](https://github.com/ricktaylor/hardy) DTN Bundle Protocol implementation.
Implements the `BundleStorage` trait from hardy-bpa, storing bundles as S3 objects with
SigV4a authentication and multipart upload for large bundles. Compatible with MinIO and
other S3-compatible stores via configurable endpoint URL and path-style addressing.

## Documentation

- [Design](docs/design.md)

## Licence

Apache 2.0 -- see [LICENSE](../LICENSE)
