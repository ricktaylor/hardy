# hardy-s3-storage Design

S3-compatible object storage backend implementing the `BundleStorage` trait.

## Design Goals

- **Shared storage for horizontal scaling.** Unlike local disk storage, S3 is accessible by all
  BPA replicas simultaneously. Bundle data written by one replica can be loaded or deleted by
  any other replica without coordination.

- **Crash safety through S3 durability.** S3 provides 11-nines durability natively. There is no
  need for fsync, atomic rename protocols, or local crash recovery. A `PutObject` that returns
  success is guaranteed to be durable.

- **S3-compatible.** Works with AWS S3, MinIO, LocalStack, and any S3-compatible object store
  via configurable endpoint URL and path-style addressing.

- **Namespace isolation.** A configurable key prefix allows multiple BPA deployments to share
  the same bucket without key collision.

## Architecture Overview

```
BPA
 |
 +- save(bundle_data)    -> PutObject (small)           uuid-v4 key -> returns storage_name
 |                       -> CreateMultipartUpload +
 |                          UploadPart x N (sequential) +
 |                          CompleteMultipartUpload (large, >= multipart_threshold)
 +- load(storage_name)   -> GetObject  -> returns bundle bytes (or None)
 +- delete(storage_name) -> DeleteObject (idempotent)
 +- recover()            -> ListObjectsV2 (paginated) -> emit (storage_name, last_modified)
```

Each bundle is stored as a single S3 object. The object key is a UUID v4, optionally prefixed
by the configured namespace prefix. The `storage_name` stored in metadata is the bare UUID
(without prefix), so the prefix can be changed without invalidating existing metadata references.

## Key Design Decisions

### UUID v4 Keys

Storage names are UUID v4 strings (e.g. `550e8400-e29b-41d4-a716-446655440000`). 128-bit random
keys make collisions negligible at any realistic bundle rate. UUIDs are scheme-agnostic and carry
no information about bundle content, source, or routing.

The localdisk backend uses a two-level directory hierarchy to avoid filesystem directory size
limits. S3 has no such constraint: any number of objects can share a common prefix. The UUID
key is flat.

### No Atomic Write Protocol

The localdisk backend uses a write-to-tmp then atomic rename protocol to guard against partial
writes. S3 `PutObject` is atomic at the API level: an object is either fully present or absent.
There is no intermediate state visible to other clients. No temporary objects or rename
operations are needed.

### Idempotent Delete

S3 `DeleteObject` returns success even when the object does not exist. The `delete()` method
exploits this: it issues the delete unconditionally and never treats a missing key as an error.
This is safe for the BPA's use case because delete is always a cleanup operation.

### `NoSuchKey` on Load

`GetObject` returns a `NoSuchKey` service error when the object does not exist. The `load()`
method maps this specifically to `Ok(None)`, matching the `BundleStorage` contract. All other
S3 errors are classified as fatal since they indicate infrastructure failure.

### All S3 Errors Are Fatal

Unlike metadata storage (where constraint violations are transient), S3 errors have no logical
failure mode at the object level. An object either exists or it does not. Any error other than
`NoSuchKey` on load indicates that the S3 service or network is unavailable, which is an
infrastructure failure. All such errors map to `storage::Error::Fatal`.

### Multipart Upload for Large Bundles

`PutObject` has a hard 5 GiB limit imposed by the S3 API. Above a configurable threshold
(`multipart_threshold`, default 8 MiB), `save()` switches to the S3 multipart upload protocol:

1. `CreateMultipartUpload` — opens the upload session and returns an `upload_id`.
2. `UploadPart` — uploads the payload in sequential chunks of `multipart_part_size` (default
   8 MiB, minimum 5 MiB per S3 constraints). `Bytes::slice` is used to reference each chunk
   without copying.
3. `CompleteMultipartUpload` — commits all parts as a single atomic object.

If any step after `CreateMultipartUpload` fails, `AbortMultipartUpload` is called before
propagating the error. This releases the incomplete parts immediately and avoids accruing
storage charges for abandoned uploads. A bucket lifecycle rule aborting stale incomplete
multipart uploads after N days is recommended as a belt-and-suspenders measure.

Parts are uploaded sequentially. Parallel part upload is a future optimisation.

### Recovery via `ListObjectsV2`

The `recover()` method pages through all objects under the configured prefix using
`ListObjectsV2`. For each object it emits a `(storage_name, last_modified)` pair to the
recovery channel. `LastModified` is used as the ingress timestamp approximation; subsecond
precision is not needed for recovery ordering.

**Shared storage implication:** because S3 is shared across all replicas, the recovery walk
lists objects written by any replica. This is correct behaviour: an orphaned object from a
crashed replica will be discovered by the recovering node and matched against metadata. See
`issues/bpa-horizontal-scaling.md` for the remaining coordination problems that S3 storage
alone does not solve (specifically: the metadata recovery protocol still assumes exclusive
access and must be redesigned separately).

## Configuration

| Option | Default | Purpose |
|--------|---------|---------|
| `bucket` | (required) | S3 bucket name |
| `prefix` | `""` | Key prefix for all objects (no leading or trailing slash) |
| `region` | env var | AWS region; falls back to `AWS_DEFAULT_REGION` / `AWS_REGION` |
| `endpoint_url` | none | Custom endpoint for S3-compatible stores (MinIO, LocalStack) |
| `force_path_style` | `false` | Path-style addressing; required for MinIO and some compatibles |
| `multipart_threshold` | 8 MiB | Bundle size above which multipart upload is used |
| `multipart_part_size` | 8 MiB | Size of each part in a multipart upload (minimum 5 MiB) |

AWS credentials are not stored in configuration. Use the standard credential chain:
`AWS_ACCESS_KEY_ID` / `AWS_SECRET_ACCESS_KEY` environment variables, an IAM instance role, or
`~/.aws/credentials`.

## Integration

### With hardy-bpa

Implements the `BundleStorage` trait. The BPA calls `save()`, `load()`, `delete()`, and
`recover()` without knowing the underlying storage mechanism. S3 storage is injected alongside
a metadata storage backend (e.g. `hardy-postgres-storage`) by the embedding application.

### With hardy-bpa-server

The server instantiates S3 storage based on configuration and injects it into the BPA. S3
storage is the bundle data backend for cloud-native deployments where multiple BPA replicas
share the same bucket.

### Pairing with postgres-storage

S3 bundle storage is designed to be paired with `hardy-postgres-storage` for the metadata
backend. Both are shared, durable, and accessible from multiple replicas simultaneously. This
combination is the intended foundation for horizontal scaling once the coordination problems
described in `issues/bpa-horizontal-scaling.md` are resolved.

## Known Limitations

### In-memory bundle constraint

`BundleStorage::load` returns `Bytes` and `BundleStorage::save` takes `Bytes`. Both are owned,
contiguous, in-memory buffers. This is a trait-level constraint that applies to all backends,
not just S3.

The consequence for large bundles:

- **`load`** buffers the entire S3 object in RAM before returning. For a 1 GiB bundle, every
  `load` call is a 1 GiB RAM spike. On memory-constrained nodes this is a hard ceiling on the
  maximum processable bundle size.

- **`save`** requires the full bundle in RAM at the call site. There is no path for a CLA
  receiving a large bundle over a network connection to stream it directly into S3 without
  buffering the whole payload first.

- **No partial read** is possible. Accessing a specific fragment of a stored bundle still
  requires loading the full object.

These constraints cannot be removed at the S3 storage layer. Fixing them requires changing the
`BundleStorage` trait to use a streaming API (`AsyncRead`/`AsyncWrite` or a `ByteStream` return
type). See `issues/bundle-storage-in-memory-constraint.md`.

## Dependencies

| Crate | Purpose |
|-------|---------|
| hardy-bpa | `BundleStorage` trait definition |
| aws-config | AWS credential and region resolution |
| aws-sdk-s3 | S3 API client |
| uuid | UUID v4 key generation |
| time | `OffsetDateTime` for recovery timestamps |
