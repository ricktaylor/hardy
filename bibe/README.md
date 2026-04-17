# hardy-bibe

Bundle-in-Bundle Encapsulation (BIBE) for the Hardy BPA.

Part of the [Hardy](https://github.com/ricktaylor/hardy) DTN Bundle Protocol implementation.

## Overview

This crate implements Bundle-in-Bundle Encapsulation (RFC 9171 Appendix B concept), enabling bundles to be tunneled through intermediate DTN networks by wrapping an inner bundle inside the payload of an outer bundle.

It uses a hybrid CLA/Service architecture: encapsulation is performed by a CLA implementation that intercepts `forward()` calls, while decapsulation is handled by a Service that receives outer bundles, extracts the inner bundle, and re-injects it into the BPA. Tunnel destinations are registered as virtual peers, making them routable via standard BPA forwarding.

**Status:** Work in progress — not yet ready for production use.

## Documentation

- [Design](docs/design.md)

## Licence

Apache 2.0 — see [LICENSE](../LICENSE)
