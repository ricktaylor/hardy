# MTCP/STCP CLA for Interop Testing

## Background

ION and D3TN do not support TCPCLv4. D3TN supports MTCP (Minimal TCP Convergence
Layer) and ION supports STCP (Simple TCP) — both are trivial length-prefixed framing
protocols with no handshake, session negotiation, keepalives, or TLS. MTCP supersedes
STCP (both authored by Scott Burleigh at JPL), but the wire formats differ.
Implementing support for these protocols is the simplest path to interop testing.

**These are not standards-track protocols** and should not be promoted as first-class
CLAs. The implementation lives entirely within `tests/` and is loaded by `hardy-bpa-server`
via the dynamic plugin system described in `/workspace/dtn/docs/hardy/bpa-server/dynamic_plugin_design.md`.

### MTCP Protocol Summary

MTCP uses bare TCP connections with CBOR byte string framing. Each bundle is
sent as a CBOR byte string (major type 2): a variable-length CBOR header
followed by the raw bundle bytes.

```
+---------------------------+--------------------+
| CBOR byte string header   | Bundle Data (var.) |
| (1-9 bytes, major type 2) |                    |
+---------------------------+--------------------+
```

The CBOR byte string header encodes the bundle length using CBOR's standard
unsigned integer encoding with major type bits set to `0x40`:

| Bundle Size         | Header Bytes | Format                       |
|---------------------|--------------|------------------------------|
| 0-23                | 1            | `0x40 \| length`             |
| 24-255              | 2            | `0x58, length`               |
| 256-65535           | 3            | `0x59, length (2B BE)`       |
| 65536-4294967295    | 5            | `0x5a, length (4B BE)`       |
| > 4294967295        | 9            | `0x5b, length (8B BE)`       |

This is confirmed by the draft spec (`references/draft-ietf-dtn-mtcpcl-01.txt`,
Section 3.2) and D3TN's implementation (`/workspace/ud3tn/components/cla/mtcp_proto.c`
— `mtcp_encode_header` creates a CBOR uint then ORs `0x40`).

**ION uses STCP, not MTCP.** ION (`/workspace/ION-DTN/`) does not implement the
MTCP spec. Instead it has `stcp` (Simple TCP) which uses a **4-byte big-endian u32**
length prefix (`htonl(bundleLength)` — see `/workspace/ION-DTN/bpv7/stcp/libstcpcla.c`).
This is NOT the same wire format as MTCP's CBOR byte string framing. The CLA must
support **both** framing modes via a config setting.

**Key properties (shared by MTCP and STCP):**
- **No contact header** (unlike TCPCLv4's `dtn!` magic + version)
- **No session negotiation** (no SESS_INIT, no keepalive, no MRU exchange)
- **No transfer segmentation** (entire bundle sent as one frame)
- **No acknowledgement** (no XFER_ACK)
- **No TLS** (spec mentions optional TLS but neither ION nor D3TN implement it)
- **Unidirectional sessions** — sender connects, sends bundle(s), receiver reads
- Each connection may carry one or more bundles sequentially
- Bidirectional exchange requires two separate TCP connections

### Wire Format Summary

| Protocol | Spec | Wire Format | Used By |
|----------|------|-------------|---------|
| **STCP** (spec) | `draft-burleigh-dtn-stcp-00` | CBOR array: `[uint(len), bstr(bundle)]` | Nobody (spec only) |
| **STCP** (ION impl) | ION source | 4-byte big-endian u32 `htonl(len)` + raw bundle | ION |
| **MTCP** | `draft-ietf-dtn-mtcpcl-01` | CBOR byte string: `bstr(bundle)` (length implicit in CBOR header) | D3TN |

ION's STCP implementation diverges from its own spec — it uses `htonl()` not CBOR.

### Reference Implementations

- **D3TN (MTCP, local)**: `/workspace/ud3tn/components/cla/mtcp_proto.c` (C codec),
  `/workspace/ud3tn/pyd3tn/pyd3tn/mtcp.py` (Python client)
- **D3TN test tools**: `/workspace/ud3tn/tools/cla/mtcp_test.py`,
  `mtcp_send_bundle.py`, `mtcp_sink.py`
- **ION (STCP, local)**: `/workspace/ION-DTN/bpv7/stcp/libstcpcla.c` (codec + send/recv),
  `/workspace/ION-DTN/bpv7/stcp/stcpcli.c` (induct daemon),
  `/workspace/ION-DTN/bpv7/stcp/stcpclo.c` (outduct daemon)

### Specifications

- **MTCP**: `references/draft-ietf-dtn-mtcpcl-01.txt` — CBOR byte string framing
- **STCP**: `references/draft-burleigh-dtn-stcp-00.txt` — CBOR array framing (spec),
  but ION's implementation uses 4-byte big-endian u32 instead

---

## Architecture: Plugin CLA in `tests/`

The MTCP/STCP CLA is **not** a top-level workspace crate. It is built as a `cdylib`
plugin and loaded dynamically by `hardy-bpa-server` using the plugin system.

```
tests/interop/
├── DTNME/                     ← existing (TCPCLv4)
├── HDTN/                      ← existing (TCPCLv4)
├── dtn7-rs/                   ← existing (TCPCLv4)
├── hardy/                     ← existing (TCPCLv4)
├── ION/                       ← new: ION interop (STCP via MTCP plugin)
│   ├── docker/
│   │   ├── Dockerfile
│   │   └── start_ion
│   ├── start_ion.sh
│   └── test_ion_ping.sh
├── ud3tn/                     ← new: µD3TN interop (MTCP via MTCP plugin)
│   ├── docker/
│   │   ├── Dockerfile
│   │   └── start_ud3tn
│   ├── start_ud3tn.sh
│   └── test_ud3tn_ping.sh
├── mtcp/                      ← MTCP/STCP CLA plugin crate (shared by ION + ud3tn)
│   ├── TODO.md                ← this file
│   ├── Cargo.toml
│   └── src/
│       ├── lib.rs             ← hardy_create_cla entry point + Cla trait impl
│       ├── config.rs          ← Config (address, framing mode, etc.)
│       ├── codec.rs           ← MTCP (CBOR bstr) and STCP (u32) codecs
│       ├── listen.rs          ← TCP listener + dispatch loop
│       └── connect.rs         ← Outbound connection + forward
├── benchmark.sh
└── README.md
```

### Plugin Loading Flow

Per the dynamic plugin design (`/workspace/dtn/docs/hardy/bpa-server/dynamic_plugin_design.md`):

1. Config file sets `type = "mtcp"` (or `"stcp"`) in `[[clas]]` — not a built-in type
2. `clas::init()` falls through to the `Plugin` catch-all variant
3. Server loads `libmtcp.so` from `plugin-dir` and calls `hardy_create_cla(config_json)`
4. Plugin returns `Arc<dyn Cla>`, server calls `bpa.register_cla()`
5. BPA calls `cla.on_register(sink, node_ids)` — CLA starts listener

Config example:
```toml
plugin-dir = "/path/to/tests/interop/mtcp/target/release"

[[clas]]
name = "mtcp0"
type = "mtcp"                  # → loads libmtcp.so from plugin-dir
address = "[::]:4556"
framing = "mtcp"               # or "stcp" for ION
```

---

## Phase 0: Prerequisites — Plugin System Infrastructure

The dynamic plugin system is currently a proposal, not implemented. The following
must be implemented first (or a simplified subset for CLA plugins only).

**Key design change**: The plugin loading code must be **shared** between
`hardy-bpa-server` and `bp ping` (and potentially other tools). The original
plugin design (`/workspace/dtn/docs/hardy/bpa-server/dynamic_plugin_design.md`)
puts the loader in `bpa-server/src/plugins.rs`, but `bp ping` also needs to load
CLA plugins. This means the host-side loading logic (`load_and_check()`,
`load_cla_plugin()`, ABI token verification) should live in `hardy-plugin-abi`
(or a sibling `hardy-plugin-host` crate), not in the server binary.

### Plugin ABI crate (`hardy-plugin-abi`)

- [ ] Create `hardy-plugin-abi` crate with:
  - **Plugin-side** (used by plugin crates):
    - `PluginResult<T>`, `PluginError`, `parse_config()`, `guard()`, `guard_factory()`
    - `ABI_TOKEN` constant
  - **Host-side** (used by `bpa-server`, `bp ping`, and other consumers):
    - `load_and_check(path) -> Result<Library>` — load `.so` and verify ABI token
    - `load_cla_plugin(path, config_json) -> Result<(Library, Arc<dyn Cla>)>` — load and call factory
    - These use `libloading`, gated behind a `host` feature flag
- [ ] The `host` feature is opt-in — plugin crates don't need `libloading`
- [ ] Plugin crates depend on `hardy-bpa` directly for trait types (`Cla`, `Sink`, etc.)
  — the ABI crate does not re-export them

### `hardy-bpa-server` integration

- [ ] Add `dynamic-plugins` feature to `hardy-bpa-server` depending on `hardy-plugin-abi/host`
- [ ] Add `Plugin` catch-all variant to `ClaConfig` enum in `bpa-server/src/clas.rs`
- [ ] Add `plugin-dir` config field to `bpa-server/src/config.rs`
- [ ] Wire `hardy_plugin_abi::load_cla_plugin()` into `clas::init()` for unknown types

### `bp ping` integration

- [ ] Add `dynamic-plugins` feature to `hardy-tools` depending on `hardy-plugin-abi/host`
- [ ] Add `--cla-plugin <path>` flag — path to a `.so` CLA plugin
- [ ] Add `--cla-config <json>` flag — JSON config string for the plugin
- [ ] When `--cla-plugin` is specified:
  - Call `hardy_plugin_abi::load_cla_plugin(path, config_json)`
  - Register the returned `Arc<dyn Cla>` with the inline BPA
  - The CLA handles its own listening (if configured) — `bp ping` just needs to
    set up a route so the BPA can forward bundles through it
- [ ] Note: unlike TCPCLv4 where `bp ping` calls `cla.connect()` to initiate an
  outbound session, MTCP/STCP have no explicit connect step — the CLA's `forward()`
  method handles connect-per-bundle. The route + destination EID is sufficient.
- [ ] The `Library` handle must be kept alive until after `bpa.shutdown()`

**Simplified bootstrap option**: If the full plugin system is too much upfront work,
implement just the CLA plugin loading path first — it's the simplest entry point
(single factory function, loaded by name or by path).

---

## Phase 1: MTCP/STCP CLA Plugin Crate (`tests/interop/mtcp/`)

### 1.1 Crate Setup

- [ ] Create `tests/interop/mtcp/Cargo.toml`:
  ```toml
  [package]
  name = "hardy-mtcp-cla"
  version = "0.1.0"
  edition.workspace = true

  [lib]
  crate-type = ["cdylib"]

  [dependencies]
  hardy-plugin-abi = { path = "../../../../plugin-abi" }
  hardy-cbor = { path = "../../../../cbor" }
  tokio = { version = "1", features = ["macros", "time", "net", "io-util"] }
  tokio-util = { version = "0.7", features = ["codec"] }
  serde = { version = "1", features = ["derive"] }
  serde_json = "1"
  tracing = "0.1"
  ```
- [ ] NOT a workspace member — built separately (avoids polluting the main workspace)

### 1.2 Plugin Entry Point (`src/lib.rs`)

- [ ] Export `HARDY_ABI_TOKEN` static
- [ ] Export `hardy_create_cla(config_json) -> PluginResult<Arc<dyn Cla>>`:
  - Parse config JSON into `Config` struct
  - Construct and return `Arc<Cla>`

### 1.3 Config (`src/config.rs`)

- [ ] `Config` struct (serde-deserializable from JSON):
  - `address: Option<SocketAddr>` — listen address (default `[::]:4556`)
  - `framing: Framing` — enum: `Mtcp` (CBOR byte string) or `Stcp` (u32 network order)
  - `max_bundle_size: u64` — reject bundles larger than this

### 1.4 Codec (`src/codec.rs`)

- [ ] `MtcpCodec` — CBOR byte string framing (for D3TN):
  - Decoder: use `hardy-cbor` pull-parser to read the CBOR byte string header
    from the `BytesMut` buffer — it handles partial reads naturally (returns
    "need more data" if the header is incomplete), then read the payload bytes
  - Encoder: use `hardy-cbor` to write CBOR byte string header + bundle bytes
- [ ] `StcpCodec` — 4-byte u32 framing (for ION):
  - Decoder: read 4-byte big-endian u32 length, read bundle bytes
  - Encoder: write `htonl(len)` + bundle bytes
- [ ] Both implement `tokio_util::codec::Decoder<Item=Bytes>` and `Encoder<Bytes>`
- [ ] Use `hardy-cbor` for all CBOR encoding/decoding (it's tuned for this use case)
- [ ] Factory function or enum dispatch to select codec based on `Framing` config

### 1.5 Listener (`src/listen.rs`)

- [ ] Bind TCP listener on configured address
- [ ] For each accepted connection, spawn a task that:
  - Wraps the socket in the framed codec (MTCP or STCP per config)
  - Reads bundles in a loop until EOF/error
  - Calls `sink.dispatch(bundle, None, Some(ClaAddress::Tcp(remote_addr)))` for each
  - For STCP: skip zero-length preambles (ION uses these as keepalives)
- [ ] No rate limiting needed — this is a test CLA
- [ ] Respect `TaskPool` cancellation

### 1.6 Connector / Forward (`src/connect.rs`)

- [ ] `forward()`: Connect to remote `SocketAddr`, write bundle using codec, close connection
- [ ] Connect-per-bundle — both protocols signal end-of-transmission by closing the connection:
  - ION STCP keeps connections alive with keepalives, but that complexity isn't needed for testing
  - D3TN MTCP closes connections after contact by default (`CLA_MTCP_CLOSE_AFTER_CONTACT=1`)
  - D3TN's `mtcp_send_bundle.py` connects, sends one bundle, disconnects
  - Connect-per-bundle is simplest, most robust, and compatible with both peers
- [ ] Handle `ClaAddress::Tcp(addr)` only; return `NoNeighbour` for others

### 1.7 CLA Trait Implementation (in `src/lib.rs`)

- [ ] Implement `hardy_bpa::cla::Cla` trait (re-exported by `hardy-plugin-abi`):
  - `on_register`: Store `Sink` + `NodeIds` in `spin::Once<Inner>`, start listener,
    and if `peer` + `peer-node` are in config, call
    `sink.add_peer(ClaAddress::Tcp(peer_addr), &[peer_node_id])` to register
    a static peer — this creates a wildcard RIB forward entry (e.g., `ipn:2.*`)
    so the BPA can route bundles to this peer without any external route setup
  - `on_unregister`: Cancel tasks, await task pool shutdown
  - `forward`: Connect to peer, send bundle

---

## Phase 2: `bp ping` Tool — Generalized CLA Selection

The `bp ping` tool (`tools/src/ping/exec.rs`) currently hardcodes TCPCLv4. It
should support a generic CLA selection mechanism that works for built-in CLAs,
future CLAs (e.g., QUIC-CL), and plugin CLAs alike.

### Unified CLI interface

```
# Built-in (default — unchanged from today)
bp ping ipn:2.7 127.0.0.1:4556

# Built-in with explicit selection and config
bp ping ipn:2.7 127.0.0.1:4556 --cla tcpclv4 --cla-config '{"require-tls":false}'

# Plugin CLA (path to .so)
bp ping ipn:2.7 --cla /path/to/libmtcp.so \
    --cla-config '{"framing":"stcp","peer":"127.0.0.1:4556","peer-node":"ipn:2.0"}'

# Future built-in
bp ping ipn:2.7 127.0.0.1:4556 --cla quicl
```

### Implementation

- [ ] Add `--cla <name-or-path>` CLI flag (default: `tcpclv4`)
  - If the value matches a built-in name (`tcpclv4`, future `quicl`, etc.),
    use that CLA directly
  - If the value is a file path (contains `/` or `.so`/`.dylib`), load as plugin
- [ ] Add `--cla-config <json>` CLI flag — raw JSON config, applicable to any CLA
  - For built-in CLAs: deserialized into the CLA's config struct (e.g.,
    `hardy_tcpclv4::config::Config`)
  - For plugin CLAs: passed verbatim to `hardy_create_cla()`
- [ ] Refactor `exec_async()` to dispatch on the `--cla` value:
  ```rust
  match cla_type {
      "tcpclv4" => { /* existing TCPCLv4 path, merge cla_config if provided */ }
      // "quicl" => { /* future */ }
      path => {
          /* load plugin via hardy_plugin_abi::load_cla_plugin(path, cla_config) */
          /* register with BPA */
          /* CLA calls sink.add_peer() from config — no explicit connect needed */
      }
  }
  ```
- [ ] For built-in CLAs: existing `peer` positional arg + `cla.connect()` flow unchanged
- [ ] For plugin CLAs: peer info is in `--cla-config` JSON, CLA handles `sink.add_peer()`
  during `on_register()`, no explicit route addition needed
- [ ] Keep `Library` handle alive until after `bpa.shutdown()`
- [ ] TLS flags (`--tls-insecure`, `--tls-ca`) remain TCPCLv4-specific — for other
  CLAs, TLS config goes in `--cla-config`

---

## Phase 3: ION Interop Tests (`tests/interop/ION/`)

ION source is available locally at `/workspace/ION-DTN/`.

**Important:** ION uses `stcp` framing (4-byte u32 length prefix). The CLA plugin
must be configured with `framing = "stcp"`.

### 3.1 Docker Image

- [ ] Create `tests/interop/ION/docker/Dockerfile`
  - Build ION from local source or `github.com/nasa-jpl/ION-DTN`
  - Include `bpadmin`, `ipnadmin`, `bpecho`, `bping`, `stcpcli`, `stcpclo`, etc.
- [ ] Create `tests/interop/ION/docker/start_ion` wrapper script

### 3.2 ION Configuration

ION uses `.rc` admin command files:
- `ionadmin` — node number, contact plan
- `bpadmin` — protocol and induct/outduct:
  - `a protocol stcp`
  - `a induct stcp <port> stcpcli`
  - `a outduct stcp <host>:<port> stcpclo`
- `ipnadmin` — EID-to-outduct routing (`a plan <node_num> stcp/<host>:<port>`)
- `ionsecadmin` — `1` to initialize security

### 3.3 Test Script

- [ ] Create `test_ion_ping.sh` following the DTNME pattern:
  - Build CLA plugin (`cargo build --release` in `tests/interop/mtcp/`)
  - Start Hardy with MTCP plugin (`type = "stcp"`, `framing = "stcp"`)
  - **TEST 1**: ION as server with `bpecho`, Hardy pings via STCP
  - **TEST 2**: Hardy as server (STCP CLA + echo service), ION `bping` pings Hardy
- [ ] Create `start_ion.sh` for interactive debugging

### 3.4 Configuration

| Parameter       | Value         | Description                      |
|-----------------|---------------|----------------------------------|
| Hardy Node      | ipn:1.0       | Hardy's administrative endpoint  |
| ION Node        | ipn:2.0       | ION administrative endpoint      |
| Hardy STCP Port | 4557          | Port Hardy listens on (TEST 2)   |
| ION STCP Port   | 4556          | Port ION listens on (TEST 1)     |
| Echo Service    | ipn:X.7       | Standard echo service number     |

---

## Phase 4: µD3TN Interop Tests (`tests/interop/ud3tn/`)

µD3TN source is available locally at `/workspace/ud3tn/`.

### 4.1 Docker Image

- [ ] Create `tests/interop/ud3tn/docker/Dockerfile`
  - Build ud3tn from local source or `gitlab.com/d3tn/ud3tn`
  - Existing Dockerfiles at `/workspace/ud3tn/dockerfiles/` may be reusable
  - Include `ud3tn` daemon and Python tools (`pyd3tn`, `ud3tn-utils`)
- [ ] Create `tests/interop/ud3tn/docker/start_ud3tn` wrapper script

### 4.2 µD3TN Configuration

- Start daemon: `ud3tn --eid ipn:2.0 --cla mtcp:<listen_addr>,<port>`
- Configure contacts via AAP2 or CLI
- µD3TN's MTCP default port is 4222 (not 4556)
- Reference tools in `/workspace/ud3tn/tools/cla/`

### 4.3 Test Script

- [ ] Create `test_ud3tn_ping.sh` following the DTNME pattern:
  - Build CLA plugin
  - Start Hardy with MTCP plugin (`framing = "mtcp"`)
  - **TEST 1**: µD3TN as server, Hardy pings via MTCP
  - **TEST 2**: Hardy as server, µD3TN pings via MTCP
- [ ] Create `start_ud3tn.sh` for interactive debugging
- [ ] Investigate µD3TN echo service availability (may need AAP agent)

---

## Phase 5: Update Documentation

- [ ] Update `tests/interop/README.md` — add ION and µD3TN sections
- [ ] Update `docs/interop_test_plan.md` — change ION/µD3TN from "File (Shared Vol)" to "MTCP/STCP"
- [ ] Do NOT add MTCP/STCP to top-level architecture docs — these are test-only CLAs

---

## Connection Lifecycle Notes

Checked ION and D3TN source code to understand connection behavior:

**ION STCP** (`/workspace/ION-DTN/bpv7/stcp/`):
- `stcpclo` (sender): maintains a **long-lived** connection with a keepalive thread
  sending zero-length preambles every 15 seconds (`STCP_KEEPALIVE_PERIOD`). Reconnects
  with exponential backoff on failure. Reuses the socket for multiple bundles.
- `stcpcli` (receiver): accepts connections, spawns per-connection threads that loop
  reading bundles until connection closes. **Skips zero-length preambles** (keepalives).
- ION default port: 4456 (`BpStcpDefaultPortNbr`).

**D3TN MTCP** (`/workspace/ud3tn/components/cla/posix/cla_mtcp.c`):
- Connections are closed after a "contact" ends by default (`CLA_MTCP_CLOSE_AFTER_CONTACT=1`).
- D3TN uses time-scheduled contacts — connection held open for the contact window,
  carrying potentially multiple bundles.
- Python tools (`mtcp_send_bundle.py`): connect → send one bundle → disconnect.

**For this test CLA**: connect-per-bundle is the right approach. No connection pooling,
no keepalives, no rate limiting. Both ION and D3TN receivers handle multiple connections
fine (they accept and spawn per-connection handlers). The STCP codec must skip zero-length
preambles on receive to handle ION keepalives if Hardy is receiving from ION's `stcpclo`.

---

## Open Questions

1. **Plugin system bootstrap**: The dynamic plugin design is a proposal, not yet
   implemented. We need at minimum the CLA plugin loading path. The key change from
   the original design: host-side loading code (`load_cla_plugin()`, ABI checks)
   lives in `hardy-plugin-abi` (behind a `host` feature), not in `bpa-server`, so
   both `bpa-server` and `bp ping` can use it.

2. **Single crate, two codecs**: The CLA plugin handles both MTCP (CBOR bstr) and
   STCP (u32) framing via a `framing` config field. The crate name `hardy-mtcp-cla`
   covers both since MTCP is the more recent spec. Alternatively, name it
   `hardy-simple-tcp-cla` to be neutral.

3. **Peer registration and routing**: With TCPCLv4, `cla.connect()` triggers SESS_INIT
   which discovers the peer's node ID and calls `sink.add_peer(cla_addr, &[peer_node_id])`.
   This creates a wildcard RIB forward entry (e.g., `ipn:2.*` → `peer_42`) because
   `add_forward()` converts the `NodeId` to an `EidPattern` with a wildcard service
   number. After that, `bp ping` adds `bpa.add_route(dest, Via(peer_node))` which
   resolves through the chain.

   With MTCP/STCP, the CLA config specifies the peer(s) statically. The CLA calls
   `sink.add_peer()` during `on_register()`, creating the same wildcard RIB entry.
   **`bp ping` does not need to add any route** — the CLA's peer registration is
   sufficient for routing to work.

   CLA config example (passed via `--cla-config`):
   ```json
   {"framing":"stcp","peer":"127.0.0.1:4556","peer-node":"ipn:2.0"}
   ```

   `bp ping` passes this JSON verbatim to the plugin — it never parses or
   composes CLA-specific config fields. This keeps the tool fully generic.

4. **ION echo service number**: ION's `bpecho` may use a different service number
   than 7. Need to verify.

5. **Workspace membership**: The CLA plugin crate should NOT be a workspace member
   to keep the main workspace clean. It's built separately by the test scripts.
