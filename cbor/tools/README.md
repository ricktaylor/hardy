# CBOR Tools

A command-line utility for inspecting and manipulating CBOR (Concise Binary Object Representation) data.

## Overview

The `cbor` tool provides commands for:

- **Inspecting** CBOR data in human-readable formats
- **Converting** between CBOR binary and CBOR Diagnostic Notation (CDN)
- **Round-trip conversion** without information loss

## Building

```bash
cargo build --release -p hardy-cbor-tools
```

The binary will be available at `target/release/cbor`.

## Quick Start

```bash
# Inspect a CBOR file (shows CDN - human-readable, lossless)
cbor inspect data.cbor

# Convert CDN text to CBOR
echo '[1, 2, 3, "hello"]' | cbor compose -o data.cbor

# Round-trip test (should preserve all data)
cbor inspect data.cbor | cbor compose | cbor inspect
```

## CBOR Diagnostic Notation (CDN)

CDN is a human-readable text representation of CBOR data defined in [RFC 8949 §8](https://www.rfc-editor.org/rfc/rfc8949.html#section-8).

### Why CDN?

- **Lossless**: Preserves all CBOR semantics (tags, types, indefinite-length containers)
- **Human-readable**: Easy to read, write, and edit
- **Round-trippable**: CBOR ↔ CDN conversion without information loss
- **Standardized**: Defined in RFC 8949

### CDN Syntax

| CBOR Type | CDN Syntax | Example |
|-----------|------------|---------|
| Unsigned integer | Decimal | `0`, `42`, `1000000` |
| Negative integer | Decimal with `-` | `-1`, `-42` |
| Float | Decimal with `.` or `e` | `1.5`, `3.14159`, `1.0e10` |
| Byte string (hex) | `h'...'` | `h'deadbeef'` |
| Byte string (base64) | `b64'...'` | `b64'SGVsbG8='` |
| Text string | `"..."` | `"hello world"` |
| Array (definite) | `[...]` | `[1, 2, 3]` |
| Array (indefinite) | `[_ ...]` | `[_ 1, 2, 3]` |
| Map (definite) | `{...}` | `{1: "a", 2: "b"}` |
| Map (indefinite) | `{_ ...}` | `{_ 1: "a"}` |
| Tagged value | `tag(value)` | `24(h'a20165...')` |
| Boolean | `true`, `false` | `true` |
| Null | `null` | `null` |
| Undefined | `undefined` | `undefined` |
| Simple value | `simple(n)` | `simple(22)` |

## Commands

### `inspect`

Inspect and display CBOR data in various formats.

**Usage:**

```bash
cbor inspect [OPTIONS] <INPUT>
```

**Options:**

- `--format <FORMAT>` - Output format (default: `diag`)
  - `diag` (or `diagnostic`) - CBOR Diagnostic Notation (human-readable, lossless)
  - `json` - JSON format (lossy - loses CBOR tags, types, etc.)
  - `hex` - Hexadecimal dump
- `-e, --decode-embedded` - Opportunistically decode byte strings as nested CBOR (tagged and untagged)
- `-o, --output <FILE>` - Output file (default: stdout)
- `<INPUT>` - Input CBOR file (use `-` for stdin)

**Examples:**

```bash
# Inspect as CDN (default - lossless)
cbor inspect bundle.cbor

# Inspect with embedded CBOR auto-decode (useful for BPv7 bundles!)
cbor inspect -e bundle.cbor

# Inspect as JSON (lossy but machine-parseable)
cbor inspect --format json data.cbor > data.json

# Inspect as hex
cbor inspect --format hex data.cbor

# From stdin
cat data.cbor | cbor inspect

# Save to file
cbor inspect data.cbor -o data.txt
```

**Embedded CBOR Detection:**

The `-e` flag is particularly useful for BPv7 bundles, which contain embedded CBOR **without tag 24**:

```bash
# Tagged embedded CBOR (tag 24)
# Without -e: shows raw bytes
$ cbor inspect data.cbor
24(h'83010203')

# With -e: shows decoded content
$ cbor inspect -e data.cbor
24([1, 2, 3])

# Untagged embedded CBOR (BPv7 style!)
# Without -e: shows raw bytes
$ cbor inspect bundle.cbor
[7, h'83010203', 0, "dtn://node1/app"]

# With -e: shows decoded content
$ cbor inspect -e bundle.cbor
[7, <<[1, 2, 3]>>, 0, "dtn://node1/app"]

# Works recursively for nested embedded CBOR
$ cbor inspect -e nested.cbor
[<<24([1, 2])>>]

# Invalid CBOR bytes fall back to hex
$ cbor inspect -e data.cbor
[h'deadbeef']

# CBOR sequences (RFC 8742) are supported
$ cbor inspect -e sequence.cbor
<<1, 2, 3, "hello", true>>

# Mixed: array with CBOR sequence
$ cbor inspect -e mixed.cbor
[7, <<1, 2, 3>>, "data"]
```

**Notation:**

- `<<item>>` - Single CBOR item decoded from untagged byte string
- `<<item1, item2, ...>>` - CBOR sequence (RFC 8742) decoded from untagged byte string
- `24(...)` - Tagged embedded CBOR (RFC 8949 tag 24)
- `h'...'` - Byte string that is not valid CBOR (or `-e` not used)

**How it works:**
When `-e` is enabled, the tool tries to decode every byte string as CBOR or a CBOR sequence. If successful, it shows the decoded content using `<<...>>` notation. Multiple items separated by commas indicate a CBOR sequence. This is perfect for BPv7, which uses untagged embedded CBOR for payloads and blocks.

**Output Comparison:**

```bash
# CDN output (lossless)
$ cbor inspect bundle.cbor
24([7, [0b0000000000000100], 0, "dtn://node1/app", ...])

# JSON output (lossy - loses tag 24, binary flags become numbers)
$ cbor inspect --format json bundle.cbor
[7, [4], 0, "dtn://node1/app", ...]

# Hex output (raw bytes)
$ cbor inspect --format hex bundle.cbor
d818a20165646e3a2f2f6e6f6465312f617070...
```

### `compose`

Convert text formats (CDN, JSON) to CBOR binary.

**Usage:**

```bash
cbor compose [OPTIONS] <INPUT>
```

**Options:**

- `--format <FORMAT>` - Input format (default: `cdn`)
  - `cdn` - CBOR Diagnostic Notation (lossless, preserves all CBOR features)
  - `json` - JSON format (lossy, convenient for simple data)
- `-o, --output <FILE>` - Output file (default: stdout)
- `<INPUT>` - Input file (use `-` for stdin)

**Examples:**

```bash
# Convert CDN file to CBOR (default format)
cbor compose test.txt -o test.cbor

# From stdin
echo '[1, 2, 3]' | cbor compose -o array.cbor

# Pipe to another command
echo 'h"deadbeef"' | cbor compose | cbor inspect
# Output: h'deadbeef'

# Convert JSON to CBOR
echo '{"name": "Alice", "age": 30}' | cbor compose --format json -o data.cbor
cat data.json | cbor compose --format json -o data.cbor

# JSON arrays work too
echo '[1, 2, 3, "hello"]' | cbor compose --format json -o array.cbor

# Create complex CBOR structures with CDN
cat > bundle.txt <<'EOF'
24([
  7,
  [0b0000000000000100],
  0,
  "dtn://node1/app",
  "dtn://node2/app",
  "dtn://node2/app",
  [1000000, 0],
  3600000000
])
EOF

cbor compose bundle.txt -o bundle.cbor
```

**Format Comparison:**

| Feature | CDN | JSON |
|---------|-----|------|
| All CBOR types | ✓ | ✗ |
| Tags | ✓ | ✗ |
| Byte strings | ✓ | ✗ |
| Indefinite-length | ✓ | ✗ |
| Negative integers | ✓ | ✓ |
| Floats | ✓ | ✓ |
| Lossless | ✓ | ✗ |
| Widely supported | ✗ | ✓ |

## Common Workflows

### Inspecting Unknown CBOR Data

```bash
# Quick inspection
cbor inspect unknown.cbor

# Detailed inspection with output saved
cbor inspect unknown.cbor -o inspection.txt

# Machine-readable format for processing
cbor inspect --format json unknown.cbor | jq .
```

### Creating Test Data

```bash
# Create a simple array (CDN)
echo '[1, 2, 3]' | cbor compose -o test.cbor

# Create from JSON
echo '{"test": true, "value": 42}' | cbor compose --format json -o test.cbor

# Create a tagged value (e.g., timestamp) - CDN only
echo '1(1609459200)' | cbor compose -o timestamp.cbor

# Create a map with various types - CDN only
echo '{1: "name", 2: h"deadbeef", 3: [1, 2, 3]}' | \
  cbor compose -o complex.cbor
```

### JSON to CBOR Workflow

```bash
# Convert existing JSON file to CBOR
cbor compose --format json config.json -o config.cbor

# Use with jq for transformations
jq '.data[] | select(.active)' data.json | \
  cbor compose --format json -o filtered.cbor

# Combine JSON and CBOR tools
curl https://api.example.com/data | \
  jq '.results' | \
  cbor compose --format json | \
  cbor inspect
```

### Round-Trip Verification

```bash
# Verify lossless conversion
cbor inspect original.cbor > original.txt
cbor compose original.txt > reconstructed.cbor
cbor inspect reconstructed.cbor > reconstructed.txt
diff original.txt reconstructed.txt

# Or in a single pipeline
cbor inspect original.cbor | cbor compose | cbor inspect
```

### Debugging BPv7 Bundles

```bash
# Inspect bundle at CBOR level
cbor inspect bundle.cbor

# Compare with semantic inspection
bundle inspect bundle.cbor

# Extract and inspect a specific block
bundle extract -b 1 bundle.cbor | cbor inspect
```

## CDN vs JSON

### When to Use CDN

Use CDN (`cbor compose` or `cbor inspect --format diag`) when you need:

- **Lossless representation** of CBOR data
- **Round-trip conversion** (CBOR ↔ text ↔ CBOR)
- **CBOR-specific features** (tags, indefinite-length, exact integer types)
- **Creating test data** that matches exact CBOR encoding
- **Full control** over CBOR encoding (byte strings, tags, etc.)

### When to Use JSON

Use JSON (`cbor compose --format json` or `cbor inspect --format json`) when you need:

- **Simple data conversion** without CBOR-specific features
- **Integration** with existing JSON workflows and tools (jq, APIs, etc.)
- **Quick prototyping** with familiar JSON syntax
- **Wide compatibility** (all programming languages support JSON)

### What JSON Loses

When using JSON format with `compose`:

- **No CBOR tags** (e.g., cannot create tag 24 for embedded CBOR)
- **No byte strings** (only text strings supported)
- **No indefinite-length** containers
- **No unsigned/negative distinction** (numbers converted based on value)
- **String keys only** in maps (JSON objects always have string keys)
- **No undefined** or simple values

When using JSON format with `inspect`:

- Tags are lost (tag 24 becomes just the inner value)
- Byte strings become base64-encoded text
- Integer types are preserved as numbers
- Indefinite-length markers are lost
- Undefined becomes null

## Round-Trip Guarantees

### Preserved in Round-Trip

✓ All CBOR major types (0-7)
✓ Tag numbers
✓ Integer signedness (unsigned vs negative)
✓ Byte strings vs text strings
✓ Definite vs indefinite-length containers
✓ Map insertion order
✓ Float precision

### May Change (Semantically Equivalent)

The following may change but are semantically equivalent:

- Float formatting (e.g., `1.5` vs `1.500000` in text)
- Integer encoding size (hardy-cbor uses canonical encoding)
- Byte string format in CDN (always output as hex, but accepts base64)
- Whitespace in CDN text

## Integration with Hardy Tools

| Tool | Purpose | Input | Output |
|------|---------|-------|--------|
| `bundle inspect` | BPv7 bundle inspection with semantic understanding | Bundle | Markdown/JSON |
| `cbor inspect` | Raw CBOR inspection with structure preservation | CBOR | CDN/JSON/Hex |
| `cbor compose` | Create CBOR from text formats | CDN/JSON | CBOR binary |

**Workflow Example:**

```bash
# Create a bundle
bundle create -s dtn://alice -d dtn://bob --payload "test" -o test.bundle

# Inspect at CBOR level
cbor inspect test.bundle > test.cdn

# Edit the CDN (e.g., change a field)
vim test.cdn

# Recreate CBOR
cbor compose test.cdn -o modified.bundle

# Verify with bundle tool
bundle validate modified.bundle
```

## Error Handling

### Parse Errors

```bash
$ echo '[1, 2, ' | cbor compose
Error: Failed to parse CDN:
Parse error at 7: unexpected end of input, expected ']'
```

### Invalid CBOR

```bash
$ echo 'ff' | xxd -r -p | cbor inspect
Error: Invalid minor-type value 31
```

## See Also

- [DESIGN.md](DESIGN.md) - Detailed design documentation
- [RFC 8949 - CBOR](https://www.rfc-editor.org/rfc/rfc8949.html)
- [RFC 8949 §8 - Diagnostic Notation](https://www.rfc-editor.org/rfc/rfc8949.html#section-8)
- [Hardy CBOR Library](../README.md)
- [Hardy BPv7 Tools](../../bpv7/tools/README.md)
