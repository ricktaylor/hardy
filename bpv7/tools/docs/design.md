# hardy-bpv7-tools Design

Command-line utilities for BPv7 bundle manipulation.

## Design Goals

- **Library validation.** Exercise the full hardy-bpv7 API as a practical test of the library's usability and completeness.

- **Unix philosophy.** Support piping and composition, reading from stdin and writing to stdout by default. Each subcommand performs one operation well.

- **Offline operation.** All operations work on bundle files without requiring a running BPA or network connectivity. Useful for testing, debugging, and batch processing.

- **Security operation support.** Full BPSec workflow including signing, verification, encryption, and decryption using JWK key format.

## Architecture

The `bundle` binary provides subcommands organised around bundle lifecycle operations:

**Creation and Inspection:**
- `create` - Build new bundles using the `Builder` API
- `inspect` - Parse and display bundle structure (markdown or JSON output)
- `validate` - Check bundle correctness
- `extract` - Retrieve block payloads (with automatic decryption if keys provided)

**Modification:**
- `rewrite` - Canonicalise bundles and remove unsupported blocks via `RewrittenBundle`
- `add-block` - Add extension blocks using the `Editor` API
- `update-block` - Modify existing block content or flags
- `update-primary` - Modify primary block fields
- `remove-block` - Remove extension blocks

**Security Operations:**
- `sign` - Add BIB integrity protection (RFC 9173 HMAC-SHA2)
- `verify` - Validate BIB signatures
- `encrypt` - Add BCB confidentiality protection (RFC 9173 AES-GCM)
- `remove-integrity` - Remove blocks from BIB target lists
- `remove-encryption` - Decrypt and remove BCB protection

## Key Management

Keys use JSON Web Key (JWK) format per RFC 7517. The tool supports both single keys and key sets (JWKS), with automatic key selection based on `kid`, `alg`, and `key_ops` fields.

This design allows the same key file to be used for both signing and encryption operations, with the tool selecting appropriate keys based on the operation being performed.

## Integration

### With hardy-bpv7

The tool directly uses the library's public API:
- `Builder` for bundle creation
- `Editor` for bundle modification
- `ParsedBundle`, `CheckedBundle`, `RewrittenBundle` for parsing
- `bpsec` module for security operations

### Testing

The tool supports component testing of the library through shell scripts that exercise complete workflows. See `tests/bundle_tools_test.sh` for examples.

## Usage

See [README.md](../README.md) for complete usage documentation including subcommand reference, key generation, and workflow examples.

## Standards Compliance

- [RFC 9171](https://www.rfc-editor.org/rfc/rfc9171.html) - Bundle Protocol Version 7
- [RFC 9172](https://www.rfc-editor.org/rfc/rfc9172.html) - Bundle Protocol Security (BPSec)
- [RFC 9173](https://www.rfc-editor.org/rfc/rfc9173.html) - Default Security Contexts
- [RFC 7517](https://www.rfc-editor.org/rfc/rfc7517.html) - JSON Web Key (JWK)
