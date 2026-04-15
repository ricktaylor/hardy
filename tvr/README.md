# hardy-tvr

Time-Variant Routing agent for Hardy DTN. Installs and withdraws routes
in the BPA on a clock, driven by contact schedules from files, gRPC
sessions, or both.

Part of the [Hardy](https://github.com/ricktaylor/hardy) DTN Bundle Protocol implementation.

## Quick Start

1. Create a contact plan file:

   ```
   # Ground station pass — daily at 08:00 UTC, 90 minutes
   ipn:2.*.* via ipn:2.1.0 cron "0 8 * * *" duration 90m bandwidth 256K

   # Permanent backbone link
   ipn:3.*.* via ipn:3.1.0 priority 10 bandwidth 10G
   ```

2. Configure hardy-tvr (`hardy-tvr.toml`):

   ```toml
   bpa-address = "http://[::1]:50051"
   contact-plan = "/etc/hardy-tvr/contacts"
   ```

3. Run:

   ```
   hardy-tvr --config hardy-tvr.toml
   ```

Routes appear in the BPA when contact windows open and are withdrawn
when they close. Bundles waiting for a destination are automatically
re-evaluated when routes appear.

## Container Image

```bash
docker pull ghcr.io/ricktaylor/hardy/hardy-tvr:latest
```

## Configuration

Configuration is read from YAML, TOML, or JSON files and environment
variables (`HARDY_TVR_` prefix, `__` for nesting). See the
[User Documentation](https://ricktaylor.github.io/hardy/configuration/tvr/)
for the full configuration reference.

## gRPC API

The TVR gRPC service is defined in [`tvr.proto`](tvr.proto). A Markdown API reference can be generated using [protoc-gen-doc](https://github.com/pseudomuto/protoc-gen-doc):

```bash
docker run --rm \
  -v $(pwd)/tvr:/protos \
  -v $(pwd)/proto:/google_rpc \
  -v $(pwd)/tvr/docs:/out \
  pseudomuto/protoc-gen-doc \
  --proto_path=/protos \
  --proto_path=/google_rpc \
  --doc_opt=markdown,api_reference.md \
  tvr.proto
```

Example session with `grpcurl`:

```bash
# Open a session and add a contact
grpcurl -plaintext -d @ [::1]:50052 tvr.Tvr/Session <<EOF
{"msg_id": 1, "open": {"name": "test", "default_priority": 100}}
{"msg_id": 2, "add": {"contacts": [{"pattern": "ipn:2.*.*", "via": "ipn:2.1.0"}]}}
EOF
```

## Documentation

- [Design](docs/design.md)
- [Test Coverage](docs/test_coverage_report.md)
- [API Reference](docs/api_reference.md) (generated)
- [User Documentation](https://ricktaylor.github.io/hardy/configuration/tvr/)

## Licence

Apache 2.0 — see [LICENSE](../LICENSE)
