# Quick Start

Get Hardy running in minutes using Docker.

## Prerequisites

- Docker and Docker Compose (v2.23+)

## 1. Clone and start

```bash
git clone https://github.com/ricktaylor/hardy.git
cd hardy
docker compose up --build -d
```

This builds and starts a BPA server with PostgreSQL metadata storage,
S3 bundle storage (MinIO), and:

- **Node ID**: `ipn:1.0`
- **TCPCLv4**: listening on port 4556
- **gRPC**: listening on port 50051
- **Echo service**: registered on IPN service 7

For a lightweight setup with in-memory storage instead:

```bash
docker compose --profile debug up --build hardy-debug
```

## 2. Verify it's running

Check the container is healthy:

```bash
docker compose ps
```

You should see the `hardy` service with status `healthy`. You can also
check the logs:

```bash
docker compose logs hardy
```

Look for `Listening on [::]:4556` (TCPCLv4) and `Listening on [::]:50051`
(gRPC) in the output.

## 3. Customise the configuration

The configuration is in [`hardy.toml`](https://github.com/ricktaylor/hardy/blob/main/hardy.toml)
at the project root. Edit it to change node settings, storage backends,
or add TLS. See the
[BPA Server Configuration](../configuration/bpa-server.md) for all
available options.

Key settings you'll want to change for a real deployment:

- **`node-ids`** -- set your node's EID (e.g. `"ipn:42.0"`)
- **`storage.metadata.type`** -- `"postgres"`, `"sqlite"`, or `"memory"`
- **`storage.bundle.type`** -- `"s3"`, `"localdisk"`, or `"memory"`

## 4. Stop

```bash
docker compose down
```

## Next steps

- [**Docker Deployment**](docker.md) -- production setup with persistent
  storage, PostgreSQL, and multiple containers
- [**BPA Server Configuration**](../configuration/bpa-server.md) --
  node identity, gRPC, services, routing, and filters
- [**Storage Backends**](../configuration/storage.md) -- choose and
  configure persistent storage
- [**CLI Tools**](../operations/tools.md) -- `bp ping`, `bundle`, and
  `cbor` commands (requires
  [building from source](https://github.com/ricktaylor/hardy#getting-started))
