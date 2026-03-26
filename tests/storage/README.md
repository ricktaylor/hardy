# Storage Tests

Runs the same suite of metadata and bundle tests against every storage backend.

## Backends

| Backend | Type | Feature flag | Infrastructure |
|---------|------|-------------|----------------|
| Memory | Metadata + Bundle | *(default)* | None |
| SQLite | Metadata | *(default)* | None |
| Local disk | Bundle | *(default)* | None |
| PostgreSQL | Metadata | `postgres` | PostgreSQL 13+ |
| S3-compatible | Bundle | `s3` | MinIO or AWS S3 |

## Running

### Default backends (no setup)

```sh
cargo test -p storage-tests
```

### PostgreSQL

Start PostgreSQL:

```sh
docker compose -f tests/compose.storage-tests.yml up -d postgres
```

`TEST_POSTGRES_URL` must point to the server without a database name. Each test creates and drops its own isolated database automatically.

```sh
TEST_POSTGRES_URL=postgresql://hardy:hardy@localhost:5432 \
  cargo test -p storage-tests --features postgres
```

### S3 / MinIO

Start MinIO:

```sh
docker compose -f tests/compose.storage-tests.yml up -d minio minio-init
```

AWS credentials are read from the standard environment variables. Each test uses a unique key prefix within the `hardy-test` bucket for isolation.

```sh
TEST_S3_ENDPOINT=http://localhost:9000 \
AWS_ACCESS_KEY_ID=minioadmin \
AWS_SECRET_ACCESS_KEY=minioadmin \
  cargo test -p storage-tests --features s3
```

### All backends

```sh
docker compose -f tests/compose.storage-tests.yml up -d

TEST_POSTGRES_URL=postgresql://hardy:hardy@localhost:5432 \
TEST_S3_ENDPOINT=http://localhost:9000 \
AWS_ACCESS_KEY_ID=minioadmin \
AWS_SECRET_ACCESS_KEY=minioadmin \
  cargo test -p storage-tests --features postgres,s3
```

## Adding a new backend

1. Add the crate dependency (optionally feature-gated) to `Cargo.toml`.
2. Add a setup function in `src/lib.rs` returning `(Guard, Arc<dyn MetadataStorage>)` or `(Guard, Arc<dyn BundleStorage>)`.
3. Register it in `tests/storage_harness.rs` using `storage_meta_tests!` / `storage_blob_tests!` (sync setup) or `storage_meta_tests_async!` / `storage_blob_tests_async!` (async setup).
