# hardy-ipn-legacy-filter

Egress filter for IPN legacy 2-element encoding.

Part of the [Hardy](https://github.com/ricktaylor/hardy) DTN Bundle Protocol implementation.
Implements the `WriteFilter` trait from hardy-bpa. On egress, rewrites IPN 3-element EIDs
to the legacy 2-element format for next-hop peers that require the older encoding.
Peer selection is driven by configurable EID patterns.

## Documentation

- [Design](docs/design.md)

## Licence

Apache 2.0 -- see [LICENSE](../LICENSE)
