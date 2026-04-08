# Storage Backends

Hardy uses a dual storage model: one backend for **bundle metadata**
(tracking state and timestamps) and another for **bundle payload data**
(the actual bytes). The two backends can be
mixed independently.

## `storage` — Cache Settings

| Key | Valid Values | Default | Description |
|-----|-------------|---------|-------------|
| `lru-capacity` | Positive integer | `1024` | Maximum bundles in the in-memory LRU cache. |
| `max-cached-bundle-size` | Positive integer (bytes) | `16384` | Maximum bundle size eligible for caching. Bundles larger than this bypass the cache. |

## `storage.metadata` — Metadata Backend

| Key | Valid Values | Default | Description |
|-----|-------------|---------|-------------|
| `type` | `memory`, `sqlite`, `postgres` | `memory` | Metadata storage engine. |

### Memory

No persistence — metadata is lost on restart. Suitable for testing only.

```yaml
storage:
  metadata:
    type: memory
```

### SQLite

The default for single-node deployments. Zero external dependencies.
Uses WAL mode for concurrent read/write access. The database is created
automatically on first run; migrations are applied automatically on
upgrade.

| Key | Valid Values | Default | Description |
|-----|-------------|---------|-------------|
| `type` | `sqlite` | - | Selects the SQLite backend. |
| `db-dir` | Directory path | OS-dependent (see below) | Directory for the database file. |
| `db-name` | Filename | `metadata.db` | Database filename. |

Default `db-dir` by platform:

| Platform | Default path |
|----------|-------------|
| Linux | `$HOME/.cache/hardy-sqlite-storage` (XDG) |
| macOS | `$HOME/Library/Caches/dtn.Hardy.hardy-sqlite-storage` |
| Windows | `C:\Users\<user>\AppData\Local\Hardy\hardy-sqlite-storage\cache` |
| Container (no `$HOME`) | `/var/spool/hardy-sqlite-storage` |

Example:

```yaml
storage:
  metadata:
    type: sqlite
    db-dir: /var/lib/hardy/db
```

### PostgreSQL

Recommended for production and multi-container deployments where the
BPA container needs to be stateless. The database and tables are created
automatically on first connection.

| Key | Valid Values | Default | Description |
|-----|-------------|---------|-------------|
| `type` | `postgres` | - | Selects the PostgreSQL backend. |
| `database-url` | PostgreSQL connection string | *Required* | Standard `postgresql://user:pass@host:port/db` connection URL. |

Example:

```yaml
storage:
  metadata:
    type: postgres
    database-url: "postgresql://hardy:secret@db.internal/hardy"
```

## `storage.bundle` — Bundle Data Backend

| Key | Valid Values | Default | Description |
|-----|-------------|---------|-------------|
| `type` | `memory`, `localdisk`, `s3` | `memory` | Bundle data storage engine. |

### Memory

No persistence — bundle data is lost on restart. Suitable for testing
only.

```yaml
storage:
  bundle:
    type: memory
```

### Local Disk

Stores bundles as individual files in a structured two-level hash
directory layout (`xx/yy/`).

| Key | Valid Values | Default | Description |
|-----|-------------|---------|-------------|
| `type` | `localdisk` | - | Selects the local disk backend. |
| `store-dir` | Directory path | OS-dependent (see below) | Root directory for bundle files. |
| `fsync` | `true`, `false` | `true` | Flush all changes to disk after each write. Set `false` for higher throughput at the risk of data loss on power failure. |

Default `store-dir` by platform:

| Platform | Default path |
|----------|-------------|
| Linux | `$HOME/.cache/hardy-localdisk-storage` (XDG) |
| macOS | `$HOME/Library/Caches/dtn.Hardy.hardy-localdisk-storage` |
| Windows | `C:\Users\<user>\AppData\Local\Hardy\hardy-localdisk-storage\cache` |
| Container (no `$HOME`) | `/var/spool/hardy-localdisk-storage` |

Example:

```yaml
storage:
  bundle:
    type: localdisk
    store-dir: /var/spool/hardy/bundles
    fsync: true
```

### Amazon S3

Stores bundles in any S3-compatible object store (AWS S3, Google Cloud
Storage, MinIO, Ceph, etc.). Credentials are provided via standard AWS
mechanisms: environment variables (`AWS_ACCESS_KEY_ID`,
`AWS_SECRET_ACCESS_KEY`), IAM instance roles, or shared credentials
file.

| Key | Valid Values | Default | Description |
|-----|-------------|---------|-------------|
| `type` | `s3` | - | Selects the S3 backend. |
| `bucket` | Bucket name | *Required* | S3 bucket for bundle storage. |
| `endpoint` | URL | AWS default | S3 endpoint URL. Set for non-AWS S3-compatible stores (MinIO, GCS, etc.). |
| `region` | AWS region string | *Required* | AWS region for the bucket. |

Example (AWS S3):

```yaml
storage:
  bundle:
    type: s3
    bucket: hardy-bundles
    region: eu-west-1
```

Example (MinIO / self-hosted):

```yaml
storage:
  bundle:
    type: s3
    bucket: hardy-bundles
    endpoint: "http://minio:9000"
    region: us-east-1
```

!!! tip
    S3 storage makes the BPA container fully stateless when combined
    with PostgreSQL metadata — ideal for auto-scaling Kubernetes
    deployments.

## Choosing a Backend

| Deployment | Metadata | Bundle Data | Notes |
|-----------|----------|-------------|-------|
| Development / testing | Memory | Memory | No persistence, zero setup |
| Single node | SQLite | Local Disk | Default, no external dependencies |
| Production (single node) | SQLite | Local Disk + fsync | Durable, simple operations |
| Cloud / multi-node | PostgreSQL | S3 | Stateless BPA, scalable |
| Hybrid | PostgreSQL | Local Disk | When S3 latency is unacceptable |

See also:

- [**BPA Server**](bpa-server.md) -- core BPA configuration
