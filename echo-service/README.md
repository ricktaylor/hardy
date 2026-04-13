# hardy-echo-service

In-process echo service for DTN testing.

Part of the [Hardy](https://github.com/ricktaylor/hardy) DTN Bundle Protocol implementation.
Implements the `Service` trait from hardy-bpa. Parses each incoming bundle, swaps
source and destination EIDs via the bpv7 Editor, and sends the bundle back to the
originator. Used by `bp ping` and interoperability testing.

## Documentation

- [Design](docs/design.md)

## Licence

Apache 2.0 -- see [LICENSE](../LICENSE)
