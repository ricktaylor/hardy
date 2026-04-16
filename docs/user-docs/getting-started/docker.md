# Docker Deployment

Container deployments for Hardy, from development to production.

## Container Images

Hardy provides the following container images, published to the GitHub
Container Registry:

| Image | Contents | Default Port |
|-------|----------|-------------|
| `ghcr.io/ricktaylor/hardy/hardy-bpa-server` | BPA server with inline TCPCLv4 | 50051 (gRPC), 4556 (TCPCLv4) |
| `ghcr.io/ricktaylor/hardy/hardy-tcpclv4-server` | Standalone TCPCLv4 CLA | 4556 |
| `ghcr.io/ricktaylor/hardy/hardy-tvr` | Time-Variant Routing agent | 50052 (gRPC) |
| `ghcr.io/ricktaylor/hardy/hardy-tools` | CLI tools (`bp`, `bundle`, `cbor`) | -- |

All images are built for `linux/amd64` and `linux/arm64`.

## Standalone Deployment

The simplest deployment runs everything in a single container. This is
suitable for development, testing, and small single-node deployments.

The repository includes a ready-to-use Compose file with persistent
storage (PostgreSQL for metadata, MinIO for bundles):

```bash
docker compose up -d
```

This starts:

- **PostgreSQL 17** -- metadata storage with a named volume
- **MinIO** -- S3-compatible bundle storage with a named volume
- **Hardy BPA** -- configured via [`bpa-server/config.yaml`](https://github.com/ricktaylor/hardy/blob/main/bpa-server/config.yaml)

For a lightweight setup with in-memory storage (no external dependencies):

```bash
docker compose --profile debug up hardy-debug
```

The configuration uses:

```yaml
storage:
  metadata:
    type: postgres
    database-url: "postgresql://hardy:hardy@postgres/hardy"
  bundle:
    type: localdisk
    store-dir: /var/lib/hardy/bundles
    fsync: true
```

!!! warning
    Change the default PostgreSQL credentials (`hardy`/`hardy`) before
    deploying to production.

### Cloud storage with S3

For cloud-native deployments, bundle payload data can be stored in any
S3-compatible object store (AWS S3, Google Cloud Storage, MinIO, etc.)
instead of local disk:

```yaml
storage:
  bundle:
    type: s3
    bucket: hardy-bundles
    endpoint: "https://s3.eu-west-1.amazonaws.com"
    region: eu-west-1
```

This eliminates the need for persistent volumes for bundle data and
allows the BPA container to be fully stateless when combined with
PostgreSQL metadata storage. Credentials are provided via standard AWS
environment variables (`AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`) or
IAM roles.

See [Storage Backends](../configuration/storage.md) for full S3
configuration options.

## Production Deployment

The [hardy-infra](https://github.com/ricktaylor/hardy-infra) repository
provides ready-to-use Docker Compose files with full observability
(OpenTelemetry Collector, Prometheus, Grafana):

```bash
git clone https://github.com/ricktaylor/hardy-infra.git
cd hardy-infra

# Persistent storage (PostgreSQL + S3)
docker compose up

# In-memory storage (ephemeral, no external deps)
docker compose -f compose.mem.yml up
```

The default deployment runs the BPA with an embedded TCPCLv4 CLA:

```mermaid
graph LR
    PEERS["DTN peers"] <-->|:4556| BPA["bpa-server<br/>(embedded TCPCLv4)"]
    BPA --- DB["PostgreSQL"]
    BPA --- S3["MinIO (S3)"]
    BPA --- OTEL["OTEL Collector"]
```

### Standalone TCPCLv4

To run TCPCLv4 as a separate container, use the `tcpcl` profile:

```bash
docker compose --profile tcpcl up
```

This adds a standalone TCPCLv4 server that connects to the BPA via
gRPC, allowing the CLA to be scaled or restarted independently:

```mermaid
graph LR
    PEERS["DTN peers"] <-->|:4556| TCPCL["tcpclv4-server"]
    TCPCL <-->|gRPC| BPA["bpa-server"]
```

### Additional Components

Hardy's gRPC architecture allows additional components to connect to
the same BPA node — multiple CLAs, a TVR routing agent, or custom
application services. Enable the required gRPC service types in the
BPA configuration:

```yaml
grpc:
  address: "[::]:50051"
  services: ["cla", "service", "routing"]
```

See the [BPA server configuration](../configuration/bpa-server.md) and
[hardy-infra README](https://github.com/ricktaylor/hardy-infra/blob/main/README.md)
for details.

## Configuration

Container configuration can be provided in three ways:

### 1. Inline in Compose (recommended for development)

Configuration is embedded directly in the Compose file using the
`configs` block. The content can be in any supported format (YAML, TOML,
or JSON):

```yaml
configs:
  hardy-config:
    content: |
      log-level: info
      node-ids: "ipn:1.0"
      # ... rest of config
```

### 2. External config file (recommended for production)

Mount a configuration file from the host:

```yaml
services:
  hardy:
    image: ghcr.io/ricktaylor/hardy/hardy-bpa-server:latest
    volumes:
      - ./config.yaml:/etc/hardy/config.yaml:ro
    environment:
      HARDY_BPA_SERVER_CONFIG_FILE: /etc/hardy/config.yaml
```

### 3. Environment variables

Individual settings can be overridden via environment variables with
the `HARDY_BPA_SERVER_` prefix:

```yaml
services:
  hardy:
    image: ghcr.io/ricktaylor/hardy/hardy-bpa-server:latest
    environment:
      HARDY_BPA_SERVER_LOG_LEVEL: debug
      HARDY_BPA_SERVER_NODE_IDS: "ipn:42.0"
```

## Health Checking

The BPA server image includes
[`grpc_health_probe`](https://github.com/grpc-ecosystem/grpc-health-probe)
for container health checks:

```yaml
healthcheck:
  test: ["CMD", "/bin/grpc_health_probe", "-addr=127.0.0.1:50051"]
  interval: 5s
  timeout: 3s
  retries: 10
  start_period: 5s
```

This is pre-configured in both Compose files.

## Ports Reference

| Port | Protocol | Component | Purpose |
|------|----------|-----------|---------|
| 4556 | TCP | TCPCLv4 | Bundle transfer (peer-to-peer) |
| 50051 | TCP | BPA gRPC | Management API (internal) |
| 50052 | TCP | TVR gRPC | Contact schedule session API |

!!! tip
    The gRPC port (50051) should generally not be exposed externally.
    It is used for internal communication between Hardy components and
    client applications on the same host or network.

## Volumes

For persistent deployments, use named volumes or host mounts:

| Volume | Purpose | Used by |
|--------|---------|---------|
| Bundle storage | Bundle payload data | `localdisk` backend |
| PostgreSQL data | Metadata database | `postgres` backend |
| TLS certificates | Server/CA certificates | TCPCLv4 with TLS |

## Next steps

- [**BPA Server Configuration**](../configuration/bpa-server.md) -- all
  available options
- [**Storage Backends**](../configuration/storage.md) -- detailed
  storage configuration
- [**Convergence Layers**](../configuration/convergence-layers.md) --
  TCPCLv4 and TLS configuration
