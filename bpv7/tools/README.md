# Bundle Tool

A command-line utility for creating, inspecting, and manipulating Bundle Protocol version 7 (BPv7) bundles as defined in [RFC 9171](https://www.rfc-editor.org/rfc/rfc9171.html).

## Table of Contents

- [Overview](#overview)
- [Building](#building)
- [Common Workflows](#common-workflows)
  - [Creating and Inspecting a Bundle](#creating-and-inspecting-a-bundle)
  - [Securing Bundles with BPSec](#securing-bundles-with-bpsec)
  - [Working with Extension Blocks](#working-with-extension-blocks)
- [Subcommand Reference](#subcommand-reference)
  - [`create`](#create)
  - [`inspect`](#inspect)
  - [`validate`](#validate)
  - [`rewrite`](#rewrite)
  - [`extract`](#extract)
  - [`add-block`](#add-block)
  - [`update-block`](#update-block)
  - [`remove-block`](#remove-block)
  - [`sign`](#sign)
  - [`verify`](#verify)
  - [`remove-bib`](#remove-bib)
  - [`encrypt`](#encrypt)
  - [`decrypt`](#decrypt)
  - [`remove-bcb`](#remove-bcb)
- [Working with Keys](#working-with-keys)
  - [Key Structure](#key-structure)
  - [Supported Algorithms](#supported-algorithms)
  - [Generating Keys](#generating-keys)
  - [Key Selection](#key-selection)
  - [Example Keys](#example-keys)
  - [Usage Examples](#usage-examples)
- [Piping and Composition](#piping-and-composition)
- [Exit Codes](#exit-codes)
- [See Also](#see-also)

## Overview

The `bundle` tool provides a comprehensive set of subcommands for working with DTN bundles:

- **Bundle Creation**: Create new bundles from payload data
- **Inspection**: Examine bundle structure and content
- **Block Manipulation**: Add, remove, and update extension blocks
- **Security Operations**: Sign, encrypt, verify, and decrypt blocks using BPSec (RFC 9172/9173)
- **Validation**: Verify bundle correctness

## Building

```bash
cargo build --release -p hardy-bpv7-tools
```

The binary will be available at `target/release/bundle`.

## Common Workflows

### Creating and Inspecting a Bundle

```bash
# Create a bundle with inline payload
bundle create \
  --source dtn://node1/app \
  --destination dtn://node2/app \
  --payload "Hello, DTN!" \
  --output hello.bundle

# Or from stdin
echo "Hello, DTN!" | bundle create \
  --source dtn://node1/app \
  --destination dtn://node2/app \
  --payload-file - \
  --output hello.bundle

# Inspect bundle (human-readable format)
bundle inspect hello.bundle

# Inspect bundle (JSON format)
bundle inspect --format json hello.bundle

# Inspect bundle (pretty-printed JSON)
bundle inspect --format json-pretty hello.bundle
```

### Securing Bundles with BPSec

```bash
# Sign block 1 (payload) using HMAC-SHA256
bundle sign -b 1 \
  --keys keys.json \
  --kid hmackey \
  --output signed.bundle \
  input.bundle

# Encrypt block 1 using AES-GCM
bundle encrypt -b 1 \
  --keys keys.json \
  --kid aesgcmkey \
  --output encrypted.bundle \
  signed.bundle

# Verify signature
bundle verify -b 1 \
  --keys keys.json \
  signed.bundle

# Decrypt block
bundle decrypt -b 1 \
  --keys keys.json \
  --output decrypted.bundle \
  encrypted.bundle
```

### Working with Extension Blocks

```bash
# Add a hop-count block
bundle add-block --type hop-count \
  --payload "hop-limit: 32" \
  --flags must-replicate \
  --output with-hop.bundle \
  input.bundle

# Update block 2's payload and flags
bundle update-block -n 2 \
  --payload "updated data" \
  --flags must-replicate,report-on-failure \
  --output updated.bundle \
  input.bundle

# Remove block 2
bundle remove-block -n 2 \
  --output cleaned.bundle \
  input.bundle
```

## Subcommand Reference

### `create`

Create a new bundle with payload.

**Usage:**

```bash
bundle create [OPTIONS] --source <EID> --destination <EID> (--payload <STRING> | --payload-file <FILE>)
```

**Required Arguments:**

- `-s, --source <EID>` - Source Endpoint ID (e.g., `dtn://node/service` or `ipn:1.2`, see [RFC 9171 §4.2.5](https://www.rfc-editor.org/rfc/rfc9171.html#section-4.2.5) and [RFC 9758](https://www.rfc-editor.org/rfc/rfc9758.html))
- `-d, --destination <EID>` - Destination Endpoint ID
- Payload (one required):
  - `-p, --payload <STRING>` - Payload as string
  - `--payload-file <FILE>` - Payload from file (use `-` for stdin)

**Optional Arguments:**

- `-r, --report-to <EID>` - Report-to Endpoint ID
- `-o, --output <OUTPUT>` - Output file (default: stdout)
- `-f, --flags <FLAGS>` - Bundle processing flags (comma-separated, see [RFC 9171 §4.2.3](https://www.rfc-editor.org/rfc/rfc9171.html#section-4.2.3)):
  - `all`, `none`
  - `admin-record`, `admin`
  - `do-not-fragment`, `dnf`
  - `ack-requested`, `ack`
  - `report-status-time`, `time`
  - `report-receiption`, `rcv`
  - `report-forwarding`, `fwd`
  - `report-delivery`, `dlv`
  - `report-deletion`, `del`
- `-c, --crc-type <TYPE>` - CRC type (see [RFC 9171 §4.2.1](https://www.rfc-editor.org/rfc/rfc9171.html#section-4.2.1)): `none`, `crc16`, `crc32`
- `-l, --lifetime <DURATION>` - Bundle lifetime (default: 24h)
- `-H, --hop-limit <COUNT>` - Maximum hop count

**Examples:**

```bash
# Create bundle with payload from file
bundle create \
  --source dtn://node1/app \
  --destination dtn://node2/app \
  --payload-file message.txt \
  --flags do-not-fragment \
  --crc-type crc32 \
  --lifetime 48h \
  --output bundle.cbor

# Create bundle with inline payload
bundle create \
  --source dtn://node1/app \
  --destination dtn://node2/app \
  --payload "Hello, DTN!" \
  --output bundle.cbor
```

---

### `inspect`

Inspect and display bundle information in various formats.

**Usage:**

```bash
bundle inspect [OPTIONS] [INPUT]
```

**Arguments:**

- `<INPUT>` - Bundle file path (use `-` for stdin)
- `--format <FORMAT>` - Output format (default: `markdown`)
  - `markdown` - Human-readable markdown format (default)
  - `json` - Machine-readable JSON format
  - `json-pretty` - Pretty-printed JSON format
- `-o, --output <OUTPUT>` - Output file (default: stdout)
- `--keys <JWKS>` - Optional key set for decrypting blocks during inspection

**Examples:**

```bash
# Display bundle in human-readable format
bundle inspect bundle.cbor

# Output as JSON for machine processing
bundle inspect --format json bundle.cbor

# Pretty-print JSON and save to file
bundle inspect --format json-pretty -o bundle.json bundle.cbor

# Inspect bundle with encrypted blocks (requires keys)
bundle inspect --keys keys.json encrypted.bundle
```

---

### `validate`

Check one or more bundles for validity.

**Usage:**

```bash
bundle validate [INPUT]...
```

**Arguments:**

- `<INPUT>...` - One or more bundle files to validate

**Example:**

```bash
bundle validate bundle1.cbor bundle2.cbor bundle3.cbor
```

---

### `rewrite`

Rewrite a bundle, removing unsupported blocks and canonicalizing.

**Usage:**

```bash
bundle rewrite [OPTIONS] [INPUT]
```

**Arguments:**

- `<INPUT>` - Bundle file path (use `-` for stdin)
- `-o, --output <OUTPUT>` - Output file (default: stdout)

**Example:**

```bash
bundle rewrite --output clean.bundle bundle.cbor
```

---

### `extract`

Extract the payload or data from a specific block.

**Usage:**

```bash
bundle extract [OPTIONS] [INPUT]
```

**Arguments:**

- `<INPUT>` - Bundle file path (use `-` for stdin)
- `-b, --block <NUMBER>` - Block number to extract (default: 1 - payload). Block 0 is the primary block, block 1 is the payload, blocks 2+ are extension blocks (see [RFC 9171 §4.1](https://www.rfc-editor.org/rfc/rfc9171.html#section-4.1))
- `-o, --output <OUTPUT>` - Output file (default: stdout)

**Example:**

```bash
# Extract payload
bundle extract bundle.cbor > payload.dat

# Extract block 3 data
bundle extract -b 3 bundle.cbor > block3.dat
```

---

### `add-block`

Add an extension block to a bundle.

**Usage:**

```bash
bundle add-block [OPTIONS] --type <TYPE> [INPUT]
```

**Required Arguments:**

- `-t, --type <TYPE>` - Block type (see [RFC 9171 §4.1](https://www.rfc-editor.org/rfc/rfc9171.html#section-4.1)):
  - `bundle-age`, `age`
  - `hop-count`, `hop`
  - `previous-node`, `prev`
  - `block-integrity`, `bib`
  - `block-security`, `bcb`
  - Numeric type code

**Block Content (one required):**

- `-p, --payload <STRING>` - Payload as string
- `--payload-file <FILE>` - Payload from file (use `-` for stdin)

**Optional Arguments:**

- `-o, --output <OUTPUT>` - Output file (default: stdout)
- `-f, --flags <FLAGS>` - Block processing flags (comma-separated, see [RFC 9171 §4.2.4](https://www.rfc-editor.org/rfc/rfc9171.html#section-4.2.4)):
  - `all`, `none`
  - `must-replicate`, `replicate`
  - `report-on-failure`, `report`
  - `delete-bundle-on-failure`, `delete-bundle`
  - `delete-block-on-failure`, `delete-block`
- `-c, --crc-type <TYPE>` - CRC type for the block
- `--force` - Replace existing block of same type if present

**Example:**

```bash
# Add a bundle-age block
bundle add-block --type bundle-age \
  --payload "12345" \
  --flags must-replicate \
  --output with-age.bundle \
  input.bundle

# Add block with force (replace if exists)
bundle add-block --type hop-count \
  --payload "data" \
  --force \
  input.bundle
```

---

### `update-block`

Update an existing block's payload, flags, or CRC.

**Usage:**

```bash
bundle update-block [OPTIONS] --block-number <NUMBER> [INPUT]
```

**Required Arguments:**

- `-n, --block-number <NUMBER>` - Block number to update

**Update Options (at least one):**

- `-p, --payload <STRING>` - New payload as string
- `--payload-file <FILE>` - New payload from file (use `-` for stdin)
- `-f, --flags <FLAGS>` - New block processing flags (comma-separated, same as add-block)
- `-c, --crc-type <TYPE>` - New CRC type

**Other Arguments:**

- `-o, --output <OUTPUT>` - Output file (default: stdout)
- `<INPUT>` - Bundle file path (use `-` for stdin)

**Example:**

```bash
# Update payload and flags of block 2
bundle update-block -n 2 \
  --payload "new data" \
  --flags must-replicate,report-on-failure \
  --output updated.bundle \
  input.bundle

# Update only CRC type
bundle update-block -n 3 \
  --crc-type crc32 \
  input.bundle
```

---

### `remove-block`

Remove an extension block from a bundle.

**Usage:**

```bash
bundle remove-block [OPTIONS] --block-number <NUMBER> [INPUT]
```

**Required Arguments:**

- `-n, --block-number <NUMBER>` - Block number to remove

**Optional Arguments:**

- `-o, --output <OUTPUT>` - Output file (default: stdout)
- `<INPUT>` - Bundle file path (use `-` for stdin)

**Example:**

```bash
bundle remove-block -n 2 --output cleaned.bundle input.bundle
```

---

### `sign`

Sign a block using BPSec Block Integrity Block (BIB) with HMAC-SHA256 (see [RFC 9173 §3](https://www.rfc-editor.org/rfc/rfc9173.html#section-3)).

**Usage:**

```bash
bundle sign [OPTIONS] (--key <JWK> | --keys <JWKS> --kid <KEY_ID>) [INPUT]
```

**Required Arguments:**

- `-b, --block <NUMBER>` - Block number to sign (default: 1)
- Key specification (choose one):
  - `--key <JWK>` - Single JWK key (JSON string or file path)
  - `--keys <JWKS> --kid <KEY_ID>` - Select key from JWKS by key ID

**Optional Arguments:**

- `-o, --output <OUTPUT>` - Output file (default: stdout)
- `-s, --source <EID>` - Security source EID (default: bundle source)
- `-f, --flags <FLAGS>` - BPSec scope flags control Additional Authenticated Data (comma-separated, see [RFC 9172 §3.6](https://www.rfc-editor.org/rfc/rfc9172.html#section-3.6)):
  - `all` - Include all in AAD (default when no flags specified)
  - `none` - Clear all flags
  - `primary` - Include primary block
  - `target` - Include target header
  - `source` - Include security source header
- `<INPUT>` - Bundle file path (use `-` for stdin)

**Examples:**

```bash
# Using a key from a JWKS file
bundle sign -b 1 \
  --keys keys.json \
  --kid hmackey \
  --flags primary,target \
  --output signed.bundle \
  input.bundle

# Using a single JWK key
bundle sign -b 1 \
  --key '{"kty":"oct","k":"..."}' \
  --output signed.bundle \
  input.bundle
```

---

### `verify`

Verify the integrity signature of a block.

**Usage:**

```bash
bundle verify [OPTIONS] --keys <JWKS> [INPUT]
```

**Required Arguments:**

- `-b, --block <NUMBER>` - Block number to verify (default: 1)
- `--keys <JWKS>` - JWKS key set (JSON string or file path)

**Optional Arguments:**

- `<INPUT>` - Bundle file path (use `-` for stdin)

**Example:**

```bash
bundle verify -b 1 \
  --keys keys.json \
  signed.bundle
```

---

### `remove-bib`

Remove the Block Integrity Block (BIB) from a signed block.

**Usage:**

```bash
bundle remove-bib [OPTIONS] --block <NUMBER> [INPUT]
```

**Required Arguments:**

- `-b, --block <NUMBER>` - Block number to remove signature from

**Optional Arguments:**

- `-o, --output <OUTPUT>` - Output file (default: stdout)
- `<INPUT>` - Bundle file path (use `-` for stdin)

**Example:**

```bash
bundle remove-bib -b 1 --output unsigned.bundle signed.bundle
```

---

### `encrypt`

Encrypt a block using BPSec Block Confidentiality Block (BCB) with AES-GCM (see [RFC 9173 §4](https://www.rfc-editor.org/rfc/rfc9173.html#section-4)).

**Usage:**

```bash
bundle encrypt [OPTIONS] (--key <JWK> | --keys <JWKS> --kid <KEY_ID>) [INPUT]
```

**Required Arguments:**

- `-b, --block <NUMBER>` - Block number to encrypt (default: 1)
- Key specification (choose one):
  - `--key <JWK>` - Single JWK key (JSON string or file path)
  - `--keys <JWKS> --kid <KEY_ID>` - Select key from JWKS by key ID

**Optional Arguments:**

- `-o, --output <OUTPUT>` - Output file (default: stdout)
- `-s, --source <EID>` - Security source EID (default: bundle source)
- `-f, --flags <FLAGS>` - BPSec scope flags (comma-separated, see [RFC 9172 §3.6](https://www.rfc-editor.org/rfc/rfc9172.html#section-3.6), same as sign)
- `<INPUT>` - Bundle file path (use `-` for stdin)

**Example:**

```bash
bundle encrypt -b 1 \
  --keys keys.json \
  --kid aesgcmkey \
  --output encrypted.bundle \
  input.bundle
```

---

### `decrypt`

Decrypt an encrypted block.

**Usage:**

```bash
bundle decrypt [OPTIONS] --keys <JWKS> [INPUT]
```

**Required Arguments:**

- `-b, --block <NUMBER>` - Block number to decrypt (default: 1)
- `--keys <JWKS>` - JWKS key set (JSON string or file path)

**Optional Arguments:**

- `-o, --output <OUTPUT>` - Output file (default: stdout)
- `<INPUT>` - Bundle file path (use `-` for stdin)

**Example:**

```bash
bundle decrypt -b 1 \
  --keys keys.json \
  --output decrypted.bundle \
  encrypted.bundle
```

---

### `remove-bcb`

Remove the Block Confidentiality Block (BCB) from an encrypted block, decrypting it.

**Usage:**

```bash
bundle remove-bcb [OPTIONS] --block <NUMBER> --keys <JWKS> [INPUT]
```

**Required Arguments:**

- `-b, --block <NUMBER>` - Block number to decrypt
- `--keys <JWKS>` - JWKS key set (JSON string or file path)

**Optional Arguments:**

- `-o, --output <OUTPUT>` - Output file (default: stdout)
- `<INPUT>` - Bundle file path (use `-` for stdin)

**Example:**

```bash
bundle remove-bcb -b 1 \
  --keys keys.json \
  --output decrypted.bundle \
  encrypted.bundle
```

## Working with Keys

BPSec operations use JSON Web Key (JWK) format as defined in [RFC 7517](https://www.rfc-editor.org/rfc/rfc7517.html).

### Key Structure

A JWK key consists of several fields that determine its usage:

**Required Fields:**

- `kty` (Key Type): `"oct"` for symmetric keys (most common for BPSec)
- `k` (Key Value): Base64url-encoded key material

**Important Optional Fields:**

- `kid` (Key ID): String identifier for the key (e.g., `"hmackey"`)
- `alg` (Algorithm): Key management/signing algorithm (see below)
- `enc` (Encryption Algorithm): Content encryption algorithm (for encryption operations)
- `key_ops` (Key Operations): Array of permitted operations (e.g., `["sign", "verify"]`)
- `use` (Public Key Use): `"sig"` for signature or `"enc"` for encryption

### Supported Algorithms

**Key/Signing Algorithms (`alg`):**

- `dir` - Direct use (key used as-is)
- `HS256`, `HS384`, `HS512` - HMAC with SHA-256/384/512
- `A128KW`, `A192KW`, `A256KW` - AES Key Wrap (128/192/256-bit)
- Combined: `HS256+A128KW`, `HS384+A256KW`, etc.

**Content Encryption Algorithms (`enc`):**

- `A128GCM` - AES-128-GCM
- `A256GCM` - AES-256-GCM

**Key Operations (`key_ops`):**

- `sign` - Create digital signatures (BIB)
- `verify` - Verify digital signatures
- `encrypt` - Encrypt content (BCB)
- `decrypt` - Decrypt content
- `wrapKey` / `unwrapKey` - Key wrapping operations

### Generating Keys

You can generate JWK keys using standard Linux utilities like `openssl`. The key material must be base64url-encoded (RFC 4648 §5).

**Generate HMAC-SHA256 Key (256-bit for signing):**

```bash
# Generate 32 random bytes and convert to base64url
K=$(openssl rand 32 | base64 | tr '+/' '-_' | tr -d '=')

# Create JWK
cat > hmac-key.json <<EOF
{
  "kty": "oct",
  "kid": "hmackey",
  "alg": "HS256",
  "key_ops": ["sign", "verify"],
  "k": "$K"
}
EOF
```

**Generate AES-128-GCM Key (128-bit for encryption):**

```bash
# Generate 16 random bytes and convert to base64url
K=$(openssl rand 16 | base64 | tr '+/' '-_' | tr -d '=')

# Create JWK
cat > aes128-key.json <<EOF
{
  "kty": "oct",
  "kid": "aes128key",
  "alg": "dir",
  "enc": "A128GCM",
  "key_ops": ["encrypt", "decrypt"],
  "k": "$K"
}
EOF
```

**Generate AES-256-GCM Key (256-bit for encryption):**

```bash
# Generate 32 random bytes and convert to base64url
K=$(openssl rand 32 | base64 | tr '+/' '-_' | tr -d '=')

# Create JWK
cat > aes256-key.json <<EOF
{
  "kty": "oct",
  "kid": "aes256key",
  "alg": "dir",
  "enc": "A256GCM",
  "key_ops": ["encrypt", "decrypt"],
  "k": "$K"
}
EOF
```

**Create a JWKS Key Set:**

```bash
# Combine multiple keys into a key set using jq
jq -n --slurpfile hmac hmac-key.json \
      --slurpfile aes128 aes128-key.json \
      --slurpfile aes256 aes256-key.json \
      '{keys: [$hmac[0], $aes128[0], $aes256[0]]}' > keys.json

# Or manually create the key set
cat > keys.json <<'EOF'
{
  "keys": [
    {
      "kty": "oct",
      "kid": "hmackey",
      "alg": "HS256",
      "key_ops": ["sign", "verify"],
      "k": "YOUR_BASE64URL_HMAC_KEY_HERE"
    },
    {
      "kty": "oct",
      "kid": "aes128key",
      "alg": "dir",
      "enc": "A128GCM",
      "key_ops": ["encrypt", "decrypt"],
      "k": "YOUR_BASE64URL_AES128_KEY_HERE"
    },
    {
      "kty": "oct",
      "kid": "aes256key",
      "alg": "dir",
      "enc": "A256GCM",
      "key_ops": ["encrypt", "decrypt"],
      "k": "YOUR_BASE64URL_AES256_KEY_HERE"
    }
  ]
}
EOF
```

**Note:** Base64url encoding differs from standard base64:

- Uses `-` instead of `+`
- Uses `_` instead of `/`
- Omits padding `=` characters

**Complete Example - Generate Keys and Use Them:**

```bash
# 1. Generate an HMAC key for signing
K=$(openssl rand 32 | base64 | tr '+/' '-_' | tr -d '=')
cat > test-hmac.json <<EOF
{
  "kty": "oct",
  "kid": "test-hmac",
  "alg": "HS256",
  "key_ops": ["sign", "verify"],
  "k": "$K"
}
EOF

# 2. Generate an AES key for encryption
K=$(openssl rand 16 | base64 | tr '+/' '-_' | tr -d '=')
cat > test-aes.json <<EOF
{
  "kty": "oct",
  "kid": "test-aes",
  "alg": "dir",
  "enc": "A128GCM",
  "key_ops": ["encrypt", "decrypt"],
  "k": "$K"
}
EOF

# 3. Create a key set
jq -n --slurpfile hmac test-hmac.json \
      --slurpfile aes test-aes.json \
      '{keys: [$hmac[0], $aes[0]]}' > test-keys.json

# 4. Use the keys with bundle tool
echo "Test message" | \
  bundle create -s dtn://alice -d dtn://bob --payload-file - | \
  bundle sign -b 1 --keys test-keys.json --kid test-hmac | \
  bundle encrypt -b 1 --keys test-keys.json --kid test-aes | \
  bundle decrypt -b 1 --keys test-keys.json | \
  bundle verify -b 1 --keys test-keys.json | \
  bundle extract
```

### Key Selection

**For signing/encryption operations:**

- With `--key <JWK>`: The provided key is used directly
- With `--keys <JWKS> --kid <KEY_ID>`: The key with matching `kid` is selected

**For verification/decryption operations:**

- With `--keys <JWKS>`: The tool automatically selects keys that:
  1. Have the required operation in their `key_ops` field
  2. Match the algorithm specified in the security block

### Example Keys

**HMAC-SHA256 Key for Signing:**

```json
{
  "kty": "oct",
  "kid": "hmackey",
  "alg": "HS256",
  "key_ops": ["sign", "verify"],
  "k": "AES256...base64url..."
}
```

**AES-GCM Key for Encryption:**

```json
{
  "kty": "oct",
  "kid": "aesgcmkey",
  "alg": "dir",
  "enc": "A128GCM",
  "key_ops": ["encrypt", "decrypt"],
  "k": "AES128...base64url..."
}
```

**JWKS Key Set:**

```json
{
  "keys": [
    {
      "kty": "oct",
      "kid": "hmackey",
      "alg": "HS256",
      "key_ops": ["sign", "verify"],
      "k": "..."
    },
    {
      "kty": "oct",
      "kid": "aesgcmkey",
      "alg": "dir",
      "enc": "A256GCM",
      "key_ops": ["encrypt", "decrypt"],
      "k": "..."
    }
  ]
}
```

### Usage Examples

```bash
# Using a single JWK from a file
bundle sign -b 1 --key mykey.jwk input.bundle

# Using a single JWK as JSON string
bundle sign -b 1 --key '{"kty":"oct","alg":"HS256","k":"..."}' input.bundle

# Using a key from a JWKS file by kid
bundle sign -b 1 --keys keys.json --kid hmackey input.bundle

# Verification automatically finds the right key based on key_ops
bundle verify -b 1 --keys keys.json signed.bundle

# Encryption with specific key
bundle encrypt -b 1 --keys keys.json --kid aesgcmkey input.bundle

# Decryption automatically finds keys with "decrypt" in key_ops
bundle decrypt -b 1 --keys keys.json encrypted.bundle
```

## Piping and Composition

The tool is designed for Unix-style composition using pipes:

```bash
# Create, sign, encrypt, and save in one pipeline
echo "Secret message" | \
  bundle create -s dtn://alice/app -d dtn://bob/app --payload-file - | \
  bundle sign -b 1 --keys keys.json --kid sign-key | \
  bundle encrypt -b 1 --keys keys.json --kid encrypt-key \
  > secure.bundle

# Decrypt, verify, and extract
bundle decrypt -b 1 --keys keys.json secure.bundle | \
  bundle verify -b 1 --keys keys.json | \
  bundle extract > message.txt
```

## Exit Codes

- `0` - Success
- Non-zero - Error (with diagnostic message to stderr)

## See Also

- [RFC 9171 - Bundle Protocol Version 7](https://www.rfc-editor.org/rfc/rfc9171.html)
- [RFC 9172 - Bundle Protocol Security (BPSec)](https://www.rfc-editor.org/rfc/rfc9172.html)
- [RFC 9173 - Default Security Contexts for BPSec](https://www.rfc-editor.org/rfc/rfc9173.html)
- [RFC 9758 - InterPlanetary Networking (IPN) Scheme](https://www.rfc-editor.org/rfc/rfc9758.html)
- [RFC 7517 - JSON Web Key (JWK)](https://www.rfc-editor.org/rfc/rfc7517.html)
