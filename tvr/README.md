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

## Configuration

Configuration is read from YAML, TOML, or JSON files and environment
variables (`HARDY_TVR_` prefix, `__` for nesting). See the
[User Documentation](https://ricktaylor.github.io/hardy/configuration/tvr/)
for the full configuration reference.

## Documentation

- [Design](docs/design.md)
- [Test Coverage](docs/test_coverage_report.md)
- [User Documentation](https://ricktaylor.github.io/hardy/configuration/tvr/)

## Licence

Apache 2.0 — see [LICENSE](../LICENSE)
