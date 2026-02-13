# hardy-eid-patterns Design

EID pattern matching per [draft-ietf-dtn-eid-pattern](https://datatracker.ietf.org/doc/draft-ietf-dtn-eid-pattern/).

## Design Goals

- **Specification compliance.** Implement the EID pattern draft specification for matching sets of Endpoint Identifiers using a compact textual syntax.

- **Efficient matching.** Support matching a single EID against a pattern without expanding the pattern into an enumerated set. A pattern like `ipn:*.0-1000.*` should match directly, not by generating every possible combination.

- **Subset validation.** Allow checking whether one pattern is a subset of another, enabling policy enforcement where administrators define permitted patterns and the system validates that requested patterns don't exceed those bounds.

- **Independent evolution.** The EID pattern specification is still a draft and may change. Separating this from hardy-bpv7 allows updates without disturbing the stable core library.

- **`no_std` compatibility.** The core IPN pattern functionality works on embedded platforms with only a heap allocator. DTN pattern support requires `std`.

## Pattern Structure

An EID pattern matches a set of Endpoint Identifiers. The structure follows a hierarchy designed for efficient representation and matching.

At the top level, `EidPattern` is either `Any` (matches all possible EIDs) or `Set` (a list of scheme-specific patterns). The `Any` variant exists as a distinct case because "match everything" is common in configuration and can be evaluated immediately without examining the EID at all.

Within a `Set`, each `EidPatternItem` handles a specific scheme. The IPN scheme uses interval-based matching where allocator, node, and service components can each be a wildcard (`*`) or a set of ranges. For example, `ipn:1.0-100.*` matches allocator 1, any node from 0 to 100, and any service number. The interval representation is compact and allows direct range-containment checks rather than enumeration.

### IPN Pattern Details

IPN patterns support three-component (`ipn:allocator.node.service`) and two-component (`ipn:node.service`) text forms. When parsing a two-component pattern, allocator 0 is implied, matching the RFC 9758 two-element CBOR encoding.

Intervals can be specified using several syntaxes:
- Single number: `5`
- Range: `5-10` (inclusive)
- Open-ended: `100+` (100 to maximum value)
- Multiple intervals: `[5,10-20,100+]`

During parsing, overlapping or adjacent intervals are automatically merged. For example, `[1-5,3-7,9]` becomes `[1-7,9]`. This normalisation simplifies subset checking and produces consistent output when serialising patterns back to text.

LocalNode EIDs (`ipn:!.<service>`) are represented internally using the sentinel value `node_number=u32::MAX` with `allocator_id=0`. The parser recognises `!` as the local node indicator and the display logic emits `!` when this sentinel is detected.

### DTN Pattern Details

The DTN scheme supports glob-style wildcards in the URI path, with `*` matching within a path segment and `**` matching across segments. DTN pattern support is behind the `dtn-pat-item` feature flag (see below).

## Subset Checking

Beyond simple matching, the library supports checking whether one pattern is a subset of another. This enables hierarchical policy enforcement.

Consider an administrative policy that allows a user to route bundles matching `ipn:1.*.*`. If that user attempts to create a route for `ipn:1.5-10.*`, the system can verify this is permitted (it's a subset). If they attempt `ipn:2.*.*`, the system can reject it (not a subset).

For interval-based IPN patterns, subset checking verifies that every interval in the candidate pattern is contained within some interval in the reference pattern. This is more complex than simple equality but enables flexible policy delegation.

## Parser Design

The pattern syntax is parsed using winnow, a parser combinator library. Parser combinators build complex parsers by composing smaller parsers - conceptually similar to building regex patterns from smaller pieces, but with the full power of a programming language for the composition logic.

The parser handles the draft specification's grammar including range syntax (`5-10`), wildcards (`*`), and scheme-specific rules. Parse errors include position information for diagnostic messages.

## Integration

### With hardy-bpv7

The library uses `bpv7::Eid` and related types for the values being matched. Pattern matching extracts the relevant components (scheme, node ID, service) from an EID and checks against the pattern's intervals or globs.

### With hardy-bpa

The Bundle Processing Agent uses EID patterns for routing table entries, administrative filtering rules, and security policy definitions. Patterns allow compact expression of "all bundles destined for this organisation's nodes" without enumerating every node.

## Dependencies

The library is marked `no_std` by default, suitable for embedded platforms with only a heap allocator.

Feature flags control optional functionality:

- **`std`**: Enables standard library support. Required by `dtn-pat-item`.
- **`dtn-pat-item`**: Enables DTN pattern support (`dtn://` URI patterns with glob matching). This feature is based on draft-sipos-dtn-eid-pattern-02, the last draft to include DTN pattern syntax before the IETF working group decided to defer DTN patterns until the `dtn:` scheme is better specified. Requires `std`.
- **`serde`**: Enables serde-based serialisation of pattern structures.

Core dependencies:
- `hardy-bpv7`: EID types for matching
- `winnow`: Parser combinators for pattern syntax
- `thiserror`: Error type derivation

Optional dependencies (behind feature flags):
- `percent-encoding`: URL encoding for DTN patterns (requires `dtn-pat-item`)
- `glob`: Glob pattern matching for DTN patterns (requires `dtn-pat-item`)
- `serde`: Serialisation framework (requires `serde`)

## Standards Compliance

- [draft-ietf-dtn-eid-pattern](https://datatracker.ietf.org/doc/draft-ietf-dtn-eid-pattern/) - EID Pattern specification (IPN patterns)
- [draft-sipos-dtn-eid-pattern-02](https://datatracker.ietf.org/doc/draft-sipos-dtn-eid-pattern/02/) - Earlier draft including DTN patterns (behind `dtn-pat-item` feature)

The library tracks the working group draft for IPN patterns. DTN pattern support is based on the earlier individual submission draft because the working group decided to defer DTN patterns until the `dtn:` URI scheme is better formalised. The DTN pattern functionality is placed behind a feature flag to reflect this deferred status.

## Testing

- [Unit Test Plan](unit_test_plan.md) - Draft-05 pattern matching logic
- [Fuzz Test Plan](fuzz_test_plan.md) - Pattern DSL parser robustness
