# hardy-ipn-legacy-filter Design

Egress filter that rewrites IPN EIDs to legacy 2-element encoding for peers that require it.

## Design Goals

- **Interoperability.** Enable Hardy to forward bundles to legacy DTN implementations that only understand the older 2-element IPN format.

- **Selective application.** Only rewrite bundles destined for specific peers, leaving other traffic in the standard 3-element format.

## Background

RFC 9758 defines two CBOR encoding formats for IPN EIDs:

- **3-element array**: `[allocator_id, node_number, service_number]` - Current standard, allocator is explicit
- **2-element array**: `[fqnn, service_number]` - Legacy format, where FQNN packs allocator and node into a single 64-bit value

Both formats represent the same Fully Qualified Node Number (FQNN). The 2-element format packs the allocator into the high 32 bits of the FQNN, while the 3-element format stores them separately.

Hardy uses 3-element encoding internally (`Eid::Ipn`). When forwarding to a peer that only understands 2-element encoding, this filter rewrites the bundle to use `Eid::LegacyIpn`, which produces the 2-element CBOR array. The FQNN is preserved - only the wire encoding changes.

## Key Design Decisions

### Egress Rewriting vs Ingress Normalisation

The filter operates at egress (outbound) rather than ingress (inbound). This preserves the original bundle encoding as long as possible and only converts when necessary for a specific peer.

The alternative would be to normalise all incoming legacy bundles to 3-element format. Hardy handles this differently: the `Eid::LegacyIpn` variant in bpv7 represents 2-element EIDs explicitly, allowing the BPA to process them without transformation.

### Pattern-Based Peer Matching

Rather than maintaining a list of peer addresses, the filter uses EID patterns to identify which next-hops require legacy encoding. This provides flexibility:

- Match specific nodes: `ipn:1.100.*`
- Match allocator ranges: `ipn:2.*.*`
- Match by service: `ipn:*.*.7`

The EID pattern matching is provided by hardy-eid-patterns.

### Bundle Rewriting via Editor

The filter uses hardy-bpv7's `Editor` to rewrite bundle EIDs. The Editor reconstructs the bundle with modified fields while preserving other content. This approach:

- Handles both source and destination EIDs in a single pass
- Preserves extension blocks and payload
- Produces valid CBOR encoding for the modified bundle

## Operation

When a bundle reaches egress:

1. Check if the next-hop matches any configured peer pattern
2. If no match, pass the bundle through unchanged
3. If matched, check if source or destination are `Eid::Ipn` (3-element)
4. Rewrite matching EIDs to `Eid::LegacyIpn` (2-element) using the Editor
5. Return the rewritten bundle data for transmission

## Configuration

The filter accepts a list of EID patterns identifying peers that require legacy encoding:

```yaml
ipn_legacy_filter:
  - "ipn:1.100.*"
  - "ipn:1.101.*"
```

If the configuration is empty, the filter does not register (no overhead when not needed).

## Integration

### With hardy-bpa

Registers at the `Egress` filter hook via `bpa.register_filter()`. The filter implements `WriteFilter`, receiving the bundle and its serialised data, and optionally returning rewritten data.

### With hardy-bpa-server

When compiled with the `ipn-legacy-filter` feature, configuration is loaded from the server's config file and the filter is registered at startup.

## Dependencies

| Crate | Purpose |
|-------|---------|
| hardy-bpa | Filter trait and registration |
| hardy-bpv7 | EID types and Editor for bundle rewriting |
| hardy-eid-patterns | Pattern matching for peer identification |

## Standards Compliance

- [RFC 9758](https://www.rfc-editor.org/rfc/rfc9758.html) - Updates to the 'ipn' URI Scheme (defines 3-element encoding and legacy 2-element compatibility)
