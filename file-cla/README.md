# hardy-file-cla

File-based Convergence Layer Adapter (CLA) for the Hardy Bundle Protocol Agent.

Part of the [Hardy](https://github.com/ricktaylor/hardy) DTN Bundle Protocol implementation.

## Installation

```toml
[dependencies]
hardy-file-cla = "0.1"
```

Published on [crates.io](https://crates.io/crates/hardy-file-cla).

## Overview

This crate provides a CLA that uses the local filesystem as a transport mechanism for DTN bundles. Bundles arriving in a watched "outbox" directory are dispatched to the BPA, while bundles forwarded by the BPA are written as files into per-peer "inbox" directories.

This is useful for testing, bridging air-gapped networks via removable media, or integrating with external tools that produce or consume raw bundle files.

## Documentation

- [Design](docs/design.md)

## Licence

Apache 2.0 -- see [LICENSE](../LICENSE)
