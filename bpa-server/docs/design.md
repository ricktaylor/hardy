# hardy-bpa-server Design

Deployable BPA server application.

## Design Goals

- **Production-ready deployment.** Provide a complete, runnable BPA application that operators can deploy directly. The server handles configuration loading, signal handling, storage backend selection, and graceful shutdown.

- **Flexible configuration.** Support multiple configuration formats (TOML, JSON, YAML) with environment variable overrides. Operators can use whichever format fits their deployment practices.

- **Feature-based composition.** Use Cargo feature flags to control which components are compiled in. Deployments can include or exclude gRPC, inline TCPCLv4, echo service, and storage backends based on requirements.

- **Standards-derived defaults.** Configuration values default to sensible values derived from relevant RFC specifications. Operators only need to override values that differ from standard recommendations.

## Application Architecture

The server is a thin wrapper that assembles components from the Hardy ecosystem:

1. Parse command line and configuration
2. Initialise storage backends based on configuration
3. Create BPA instance with configured storage
4. Load static routes from configuration
5. Register optional services (echo)
6. Register CLAs from configuration
7. Start optional gRPC server
8. Wait for shutdown signal
9. Gracefully shut down all components

### Startup Sequence

```
Config → Storage → BPA → Routes → Services → CLAs → gRPC → Running
```

Each component initialises in dependency order. The BPA needs storage before it can start. Routes and services register with the BPA. CLAs register and begin accepting bundles.

### Shutdown Sequence

Signal handlers listen for SIGTERM and SIGINT. On receipt, the cancellation token is triggered, causing all background tasks to begin graceful shutdown. The server waits for all tasks to complete, then shuts down the BPA (which flushes storage).

## Configuration

Configuration uses the `config` crate, which provides layered configuration from multiple sources:

1. **Default values** - Built into the application
2. **Configuration file** - TOML, JSON, or YAML
3. **Environment variables** - `HARDY_BPA_SERVER_*` prefix

Later sources override earlier ones, so environment variables can override file-based settings without modifying files.

### Configuration File Location

The server searches for configuration in platform-specific locations:
- Linux (packaged): `/home/<user>/.config/hardy-bpa-server/hardy-bpa-server.yaml`
- Linux (non-packaged): `/etc/opt/hardy-bpa-server/hardy-bpa-server.yaml`
- macOS: `~/Library/Application Support/com.dtn.Hardy.hardy-bpa-server/hardy-bpa-server.yaml`
- Windows: `%APPDATA%\Hardy\hardy-bpa-server\config\hardy-bpa-server.yaml`

Command line `-c` or `HARDY_BPA_SERVER_CONFIG_FILE` environment variable override the default location.

### Storage Configuration

Storage backends are selected in configuration:

```yaml
metadata_storage:
  type: sqlite  # or "memory"

bundle_storage:
  type: localdisk  # or "memory"
```

Each backend type supports its own configuration options. Memory backends are useful for testing; production deployments typically use sqlite for metadata and localdisk for bundles.

## Feature Flags

Cargo features control which components are compiled:

- **grpc** - Include gRPC server for remote CLA/service connections
- **sqlite-storage** - SQLite metadata storage backend
- **localdisk-storage** - Filesystem bundle storage backend
- **tcpclv4** - Inline TCPCLv4 CLA (no separate process needed)
- **file-cla** - File-based CLA for testing and air-gapped transfers
- **echo** - Built-in echo service for testing
- **ipn-legacy-filter** - Filter for legacy two-element IPN encoding
- **otel** - OpenTelemetry observability integration
- **packaged-installation** - Adjusts default configuration paths for system package installations (e.g., `/etc/hardy-bpa-server/` instead of `/etc/opt/hardy-bpa-server/` on Linux)

A minimal deployment might use only in-memory storage. A full deployment might include all features for production operation.

## gRPC Server

When the `grpc` feature is enabled, the server hosts gRPC services defined in hardy-proto:

- **Application** service for user applications
- **Service** service for system services
- **Cla** service for convergence layer adaptors

Remote CLAs and services connect to these endpoints. The gRPC module translates between protobuf messages and BPA trait calls.

## Static Routes

File-based static routing with hot-reload support. See [Static Routes Design](static_routes_design.md) for details.

## Command Line Options

- `-h, --help` - Display help
- `-v, --version` - Display version
- `-c, --config FILE` - Use specified configuration file
- `-u, --upgrade-store` - Upgrade storage format on startup
- `-r, --recover-store` - Attempt recovery of damaged storage records

The upgrade and recovery flags support maintenance operations when storage formats change between versions.

## Integration

### With hardy-bpa

The server creates a `Bpa` instance and registers components with it. The BPA handles all bundle processing; the server provides configuration and lifecycle management.

### With Storage Backends

Storage trait implementations are created based on configuration and injected into the BPA. The server is the assembly point where concrete implementations meet abstract traits.

### With hardy-otel

When the `otel` feature is enabled, the server initialises OpenTelemetry for logs, traces, and metrics. Observability data flows to configured collectors.

### With hardy-tcpclv4

When the `tcpclv4` feature is enabled, TCPCLv4 can run in-process. This avoids the overhead of gRPC communication for deployments where a separate CLA process isn't needed.

## Testing

- [Test Plan](test_plan.md) - Application lifecycle, configuration parsing, OCI packaging
