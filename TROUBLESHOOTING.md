# Troubleshooting

## BPA Server

### `Failed to connect to Postgres metadata store: Migration(VersionMissing(1))`

The database has not been migrated yet. The server defaults to validation-only mode
and will not run migrations unless explicitly told to.

Pass the `-u` flag on first start (or after upgrading):

```bash
hardy-bpa-server -u
```

In Docker Compose:

```yaml
services:
  hardy:
    image: ghcr.io/ricktaylor/hardy/hardy-bpa-server:latest
    command: ["-u"]
```

The flag is safe to leave permanently — migrations are idempotent.

## Hardy Tools

### `Failed to parse peer address 'hostname:4556': invalid socket address syntax`

The `bp` binary expects an IP address, not a hostname. Use the numeric address instead:

```bash
# Won't work
bp ping ipn:1.7 localhost:4556
bp ping ipn:1.7 hardy:4556

# Works
bp ping ipn:1.7 127.0.0.1:4556
bp ping ipn:1.7 192.168.1.10:4556
```

See [bp-ping(1)](tools/docs/bp-ping.1.md) for full usage documentation.

### Running `hardy-tools` via Docker

Use `--network host` to reach services on the host:

```bash
docker run --rm --network host ghcr.io/ricktaylor/hardy/hardy-tools:latest bp ping ipn:1.7 127.0.0.1:4556
```

Or use the compose network name to reach services by container IP:

```bash
docker run --rm --network <network_name> ghcr.io/ricktaylor/hardy/hardy-tools:latest bp ping ipn:1.7 <container-ip>:4556
```
