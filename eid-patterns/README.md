# hardy-eid-patterns

EID pattern parsing and matching for [draft-ietf-dtn-eid-pattern](https://datatracker.ietf.org/doc/draft-ietf-dtn-eid-pattern/).

Part of the [Hardy](https://github.com/ricktaylor/hardy) DTN Bundle Protocol implementation.

## Overview

hardy-eid-patterns implements the EID Pattern specification used to match Bundle Protocol Endpoint Identifiers. It supports both `ipn` and `dtn` URI schemes, providing pattern parsing from text, EID matching, specificity scoring for route selection, and subset operations for pattern comparison.

The crate is used by hardy-bpa for route matching and by hardy-tvr for contact plan pattern evaluation.

## Features

- Parse EID patterns from text (`"ipn:1.*.*"`, `"dtn://node/**"`, `"*:**"`)
- Match patterns against `Eid` values
- Specificity scoring -- higher scores for more specific patterns, enabling best-match route selection
- Subset testing -- determine if one pattern is contained within another
- Union sets via `|` separator (`"ipn:1.*.0|ipn:2.*.*"`)
- `no_std` compatible with `alloc` (default configuration)
- Feature flag: `std` -- enables standard library support across dependencies
- Feature flag: `dtn-pat-item` -- enables `dtn` scheme pattern matching with glob support (implies `std`)
- Feature flag: `serde` -- enables `Serialize` / `Deserialize` derives for `EidPattern`

**Note:** DTN glob matching is a non-standard simplification. Early drafts of the EID pattern specification included regex-based DTN matching, which was removed before WG adoption. The glob approach provides a practical approximation.

## Usage

```rust
use hardy_eid_patterns::EidPattern;

// Parse a pattern from text
let pattern: EidPattern = "ipn:1.*.*".parse().unwrap();

// Match against an EID
use hardy_bpv7::eid::Eid;
let eid = Eid::Ipn {
    fqnn: hardy_bpv7::eid::IpnNodeId { allocator_id: 1, node_number: 5 },
    service_number: 0,
};
assert!(pattern.matches(&eid));

// Specificity scoring for route selection
let broad: EidPattern = "ipn:*.*.*".parse().unwrap();
let narrow: EidPattern = "ipn:1.5.*".parse().unwrap();
assert!(narrow.specificity_score().unwrap() > broad.specificity_score().unwrap());

// Subset testing
assert!(narrow.is_subset(&broad));
```

## Documentation

- [Design](docs/design.md)
- [API Documentation](https://docs.rs/hardy-eid-patterns)
- [Test Coverage](docs/test_coverage_report.md)

## Licence

Apache 2.0 -- see [LICENSE](../LICENSE)
