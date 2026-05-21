# Design: Streaming Bundle Pipeline

| Document Info | Details |
| --- | --- |
| **Component** | BPA — Bundle data I/O, spool-based streaming |
| **Scope** | Transformer-based streaming, spool commit model, sequential-only storage |
| **Status** | Design notes — pre-implementation |
| **Related** | `queue_architecture.md`, `storage_subsystem_design.md`, Editor (`bpv7/src/editor.rs`) |

## 1. Background

This design followed an Aqueduct PoC that proposed a `Retention` trait
as a unified storage abstraction, with "bundle identity is retention
identity." The PoC correctly identified Hardy's full-bundle buffering
during ingress and egress as the limiting factor; the streaming I/O
insight — payload bytes flowing to/from storage without buffering the
whole bundle — is sound and adopted here.

The Retention abstraction itself does not fit:

- **Metadata/data separation.** `MetadataStorage` owns identity, status,
  queue assignment, and indexed queries; `BundleStorage` owns raw bytes.
  Collapsing these loses indexed query without scanning data.
- **Surgical block editing.** The Editor mutates extension blocks, BPSec
  blocks, and primary blocks without touching payload. Treating a
  retention as opaque wire-format would force whole-bundle rewrites.
- **Explicit crash recovery and BPSec access.** Hardy's three-phase
  recovery requires distinguishing committed data from in-flight writes;
  BPSec verification needs structured access to header bytes during
  streaming. The Retention proposal addresses neither.

The Retention API surface is similar to what we adopt — `reader(offset, len)`
maps to sequential read, `Write::write_all()` to spool write, `discard()`
to spool abort — but the architecture underneath is different. This
design takes the streaming-I/O kernel and implements it as improvements
to Hardy's existing architecture.

## 2. Architectural Model

### 2.1. The BPA Does Not Pass Bundles Around

The BPA is a stateless pipeline over durable state. Bundle data and
metadata both live in storage backends. The pipeline passes keys and
lightweight metadata views — indices into durable state, not the state
itself. Processing blocks dequeue a key, read what they need from
storage, make a decision, and enqueue the key to the next queue.

The total data under management may far exceed available RAM. Design
proposals should be evaluated against this model: if a concept requires
in-memory ownership of bundle data, it does not fit.

### 2.2. The Metadata/Data Split Is Preserved

`MetadataStorage` owns bundle identity, status, and queue assignment.
`BundleStorage` owns raw bytes. These remain separate traits with
independent implementations.

### 2.3. Bundle as Block Index

The `Bundle` struct remains **concrete** — a `HashMap<u64, Block>`
recording the structural index of blocks in the wire-format data: byte
extents, block types, flags, CRC types. It does not hold decoded block
content or primary block field values. Semantic interpretation
(previous node, hop count, source, destination) is a BPA concern; the
BPA extracts these during ingress and stores them in `BundleMetadata`
for indexed queries and routing.

The Editor and filter implementations plan against this index and
produce **Transformers** (§7.1) — push-based streaming processors that
receive stored bytes and emit transformed bytes. Block data is not
accessed via the `Bundle` struct or a trait; it flows through the
Transformer at execution time. Full struct details and crypto
readiness in §8.

### 2.4. Sequential Write, Sequential Read

Bundle data I/O is fully sequential in both directions:

- **Write (ingress)**: bytes arrive from the CLA in order and are
  written to storage sequentially. This is a **spool** — data flows
  through, is committed atomically, and is never modified in place.
- **Read (egress)**: stored bytes are read sequentially and pushed
  through a Transformer chain. The Transformer captures header data as
  it flows past (for CRC computation, BPSec IPPT/AAD construction),
  substitutes or injects modified blocks, and passes through unchanged
  blocks. No random access is required.

Sequential writes are the best-case I/O pattern for every backend
(disk, SSD, NOR flash, S3). The sequential write model also enables
**early drop**: the BPA can parse headers from the accumulation buffer
and run early filters before the payload arrives. If rejected, the CLA
cancels the transfer mid-stream — the payload is never received or
stored (§6.4).

## 3. The 1GB Bundle Problem

### 3.1. Current Bottlenecks

| # | Stage | Bottleneck | Memory |
|---|-------|-----------|--------|
| 1 | CLA reception | TCPCLv4 reassembles entire bundle before dispatch | 1GB alloc |
| 2 | Parse | `RewrittenBundle::parse(&data)` needs complete `&[u8]` | 1GB resident |
| 3 | Storage write | `BundleStorage::save(data)` — 1GB write while bundle is in RAM | 1GB I/O |
| 4 | Filter reject | Cannot reject until after parse — all 1GB received and stored | Wasted |
| 5 | Storage read | `BundleStorage::load()` reads entire 1GB back into memory | 1GB alloc |
| 6 | Editor flatten | `flatten`/`flatten_inplace()` may need second 1GB if buffer is shared | 1-2GB |
| 7 | CLA forward | `CLA::forward(Bytes)` holds 1GB while transmitting | 1GB resident |

### 3.2. With This Design

| # | Stage | Change | Memory |
|---|-------|--------|--------|
| 1 | CLA reception | Chunks arrive via channel, no reassembly | Bounded |
| 2 | Parse headers | Streamed parser, headers only | Kilobytes |
| 3 | Early filter | Runs after headers, before payload | — |
| 4 | Payload to storage | Spools from CLA through pipeline to disk | Bounded |
| 5 | Egress | Stored bytes stream through Transformer to CLA | Bounded |
| 6 | CLA forward | Chunks via channel, no contiguous buffer | Bounded |

**Peak memory for 1GB bundle: kilobytes** (header blocks in the
parser's accumulation buffer or cache). The payload spools sequentially
from CLA to disk at ingress and from disk to CLA at egress.

## 4. BundleStorage Trait

### 4.1. Trait Surface

The trait defines three primitives — store, load, delete — using the
stream traits from §5:

```rust
#[async_trait]
trait BundleStorage {
    /// Pull bundle bytes from `stream` into a new spool, committing
    /// atomically when the stream closes (returns `None`). Returns
    /// the assigned storage name. Cancellation via the token aborts
    /// and discards staged data.
    async fn store(
        &self,
        stream: &dyn Receiver<Bytes>,
        cancel: CancellationToken,
    ) -> Result<Arc<str>>;

    /// Push the stored bundle bytes for `storage_name` into `stream`
    /// in order, returning when all bytes have been pushed or the
    /// consumer has gone away (`SendError`).
    async fn load(
        &self,
        storage_name: &str,
        stream: &dyn Sender<Bytes>,
    ) -> Result<()>;

    /// Delete stored bundle data.
    async fn delete(&self, storage_name: &str) -> Result<()>;
}
```

Both `store` and `load` are fully sequential: every backend (local
disk, SSD, NOR flash, S3, tape) handles these primitives naturally.
The stream traits (§5) decouple the byte channel from the trait
surface — each backend wraps whatever transport is most convenient.

The `store` / `load` names match Hardy's existing storage convention
and are unambiguous because the BPA is always the caller. Symmetric
byte streaming on the CLA and filter trait surfaces (§6.1, §7.2) uses
`write(&dyn Receiver<Bytes>)` instead — there the verb describes
what the caller of the method is doing (§5).

### 4.2. Cancellation

The `CancellationToken` (from `hardy-async`) passed to `store()`
allows the BPA to abort a spool write that is no longer needed. The
backend checks the token between pulls; if cancelled, it discards the
temp file and returns an error.

Two cases trigger cancellation:

- **Generational superseding**: a later generation's write filters
  complete and commit before an earlier generation's spool write
  finishes. The earlier generation is now obsolete — cancel its write.
- **Forward-before-store**: the bundle is routed and streamed to the
  CLA (from the tee'd ingress data, §6.7) before the spool write
  completes. If the CLA confirms delivery and the bundle is
  tombstoned, the spool write can be cancelled — the data will be
  immediately deleted anyway.

The second case is the hot forward path optimisation: a small bundle
tee'd during ingress may be forwarded and acknowledged before its
spool write finishes.

### 4.3. GenerationGuard

An RAII guard wraps the cancellation token and the in-flight `store()`
task for each generational write:

```rust
struct GenerationGuard {
    cancel: CancellationToken,
    sender: Sender<Bytes>,            // producer side fed into store()
    handle: JoinHandle<Result<Arc<str>>>,
}

impl GenerationGuard {
    /// Close the producer channel and await the spool task. Returns
    /// the assigned storage_name on commit.
    async fn commit(self) -> Result<Arc<str>>;
}
```

- **On commit**: drops the producer channel (closing the `Receiver`
  the spool task is reading from), awaits the spool task. The store
  call returns the `storage_name`. The caller updates metadata and
  deletes the previous generation.
- **On drop without commit**: cancels the token, aborting the spool
  task. The backend discards the temp file.
- **On superseding**: a later generation's guard commits first and
  cancels this guard's token. The store task returns an error and
  the guard drops cleanly.

The guard ensures no leaked temp files and no explicit cleanup paths —
commit or cancel, nothing in between.

## 5. Stream Traits: Sender and Receiver

The pipeline streams items between components without coupling the
trait surface to a specific channel implementation. The storage
subsystem already establishes the **`Sender<T>` pattern** for this —
see [storage_subsystem_design.md](storage_subsystem_design.md)
§"Streaming results via `Sender<T>`" for the canonical definition
and rationale. This section reuses that pattern across the BPA's
storage, CLA, and filter trait surfaces.

The push-side trait is `Sender<T>`, already in storage:

```rust
/// Push-side stream: producer drives delivery item-by-item.
#[async_trait]
pub trait Sender<T>: Send + Sync {
    /// Returns `Err(SendError(item))` when the consumer has
    /// gone away. Producers should treat this as a definitive
    /// "stop streaming" signal, not a transient error. The rejected
    /// item is returned so the producer can recover ownership.
    async fn send(&self, item: T) -> Result<(), SendError<T>>;
}
```

The pull-side dual is `Receiver<T>` — reserved name in the storage
docs, defined here for the byte-streaming pipeline:

```rust
/// Pull-side stream: consumer drives.
#[async_trait]
pub trait Receiver<T>: Send + Sync {
    /// Returns `None` when the producer has finished or the channel
    /// has been closed.
    async fn recv(&self) -> Option<T>;
}
```

Both traits are passed as `&dyn Sender<T>` / `&dyn Receiver<T>`,
preserving object safety on the traits that consume them
(`BundleStorage`, `MetadataStorage`, `Cla`, `ReadFilter`,
`WriteFilter` — all held as `Arc<dyn ...>` chosen at runtime). The
cost of `&dyn` is one indirect call per item, negligible relative to
underlying transport work.

Channel adapters live separately from the traits. The storage
subsystem provides `ChannelSender<T>` wrapping
`hardy_async::channel::Sender<T>`; the mirror `ChannelReceiver<T>`
wraps `hardy_async::channel::Receiver<T>`. Tests use
`Mutex<Vec<T>>`-backed collectors. None of these adapters appear in
the trait surface.

Byte streams use `Sender<Bytes>` and `Receiver<Bytes>` directly —
no specialised aliases.

**Direction conventions for trait method parameters:**

- A method that takes `&dyn Sender<T>` *pushes items into it*; the
  caller holds the receiving end. Example:
  `BundleStorage::load(_, stream: &dyn Sender<Bytes>)` — storage
  pushes bytes; the caller wraps a channel receiver and reads from it.
- A method that takes `&dyn Receiver<T>` *pulls items from it*; the
  caller holds the producing end. Example:
  `BundleStorage::store(stream: &dyn Receiver<Bytes>, _)` — storage
  pulls bytes; the caller wraps a channel sender and writes into it.

**Method naming on symmetric trait surfaces.** When a trait surface is
implemented on either side of a connection (CLA traits, filter
traits), `stream_in()` / `stream_out()` are ambiguous — "in" relative
to whom? These traits use `write()` for the push direction and
`read()` for the pull direction, naming what the *caller of the
method* is doing. A CLA that calls `sink.write(stream)` is writing
bytes; the BPA that calls `cla.write(stream)` is also writing bytes.
The trait surface is identical; only the direction of data flow
differs by use site.

Storage avoids the ambiguity entirely because the BPA is always the
caller — `store()` / `load()` therefore stay with their existing
Hardy names.

## 6. Ingress: CLA to Storage

### 6.1. CLA Chunk Delivery

The existing `Sink::dispatch()` method (which accepts complete `Bytes`)
is retained for backwards compatibility. A new streaming variant uses
the `Receiver<Bytes>` pattern from §5:

```rust
trait Sink {
    // Existing — full bundle in memory
    async fn dispatch(&self, data: Bytes, ...) -> Result<()>;

    // New — streaming dispatch. CLA passes a Receiver<Bytes>
    // through which the BPA pulls chunks until the stream closes.
    async fn write(
        &self,
        stream: &dyn Receiver<Bytes>,
        ...
    ) -> Result<()>;
}
```

Streaming CLAs (e.g., TCPCLv4) construct a bounded channel, wrap the
receiver as `ChannelReceiver<Bytes>`, spawn a task that pushes each
transfer segment into the sender, and call `sink.write(&stream, ...)`
to drive ingest. Backpressure propagates naturally through the
bounded channel to TCP flow control. End of bundle is signalled by
the CLA dropping the sender — the `Receiver` then returns `None`,
`write()` finalises ingress, and returns.

If the BPA rejects mid-stream (e.g., early filter rejection),
`write()` returns early; the CLA's pushing task sees `SendError`
on its next push and aborts the transfer (e.g., TCPCLv4 XFER_REFUSE).

The BPA's `Sink` impl wraps `dispatch()` internally — it constructs a
single-item `Receiver<Bytes>` containing the `Bytes`, then calls
`write()`. The BPA core only implements the streaming ingress path.

**Transitional convenience.** Retaining both `dispatch()` / `write()`
(and likewise `Cla::forward()` / `Cla::write()` in §7.2) is
transitional. Every existing CLA reads from a network stream and
artificially materialises the full bundle before calling the
non-streaming variant; all would benefit from migrating. Once
migration is complete, the non-streaming methods can be removed.

### 6.2. Streamed Parser

The parser consumes chunks incrementally as the BPA pulls them from
the ingress `Receiver`:

1. Accumulate chunks in a small buffer
2. Parse the primary block
3. Parse extension blocks as they arrive
4. On `NeedMoreData(n)` — pull more chunks from the stream
5. Detect non-canonical encoding (flag, do not rewrite)
6. Record the payload block's offset (and length, if definite-length
   encoded) — do not parse its content

The CBOR layer's `NeedMoreData(usize)` already tells the parser how
many more bytes are needed. Inner parsing functions don't change —
they still operate on `&[u8]` slices of the accumulation buffer. The
streamed parser manages the accumulation and retry loop. Header blocks
are kept in the buffer for the duration of Phase A, giving random
access for CRC validation and filter inspection without storage I/O.

#### 6.2.1. BPSec Verification as an Early Filter

**Keyed** BPSec operations (BIB hash verification, BCB decryption /
tag verification) are **not** performed by the parser. They run as
early filters, keeping key material and security policy out of the
parser.

**Keyless** structural validation of BPSec blocks **is** performed
by the parser. RFC 9172 defines a set of inter-block structural
rules — BCB MUST NOT target the primary block (§3.7), BCB MUST NOT
target another BCB (§3.7), BIB MUST NOT target a BCB (§3.9), each
block at most one BCB target (§3.9), BCBs MUST NOT set
`delete_block_on_failure` (§3.7), BCBs targeting the payload MUST
set `must_replicate` (§3.7), security operations unique per
`(service, target)` (§2.6), every targeted block number must exist
in the bundle — that are pure functions of the parsed `Bundle`
index plus the Abstract Security Block contents of each BIB/BCB.
No keys, no policy, no I/O.

The parser memoises BIB/BCB block numbers as it walks (zero cost
for bundles with no security blocks) and, on transition to the
terminal state, decodes each ASB via `bpv7::bpsec` and applies the
rules. Violations fail the parse before the early-filter gate
runs, before any spool is opened, before any storage is touched.
This is the earliest a structurally-malformed bundle can be
rejected, and it's also reusable by offline tooling that wants the
same structural-conformance check without standing up a filter
pipeline. The same validator is exposed as a public function for
that purpose.

The early BPSec filter (running after the parser, with key access)
has access to the accumulation buffer (all header blocks in
memory) and the `Bundle` block index for structural navigation.
Header-block BIBs are verified immediately. Payload-block BIBs
require payload data — these are either deferred to a late filter
or verified inline as a stream processor during payload spooling
(§6.5).

The split — keyless-structural in the parser, keyed-cryptographic
in the filter — is the cleanest mapping of "what changes between
deployments" onto layer boundaries. The structural rules are
RFC-mandated and identical for every implementation; keys, key
sources, and security policy vary by deployment.

#### 6.2.2. Separation of Parsing and Canonicalisation

The current parser's `RewrittenBundle::Rewritten` path conflates
parsing with mutation — it canonicalises non-canonical CBOR and fixes
up blocks during parsing, returning the rewritten bundle as
`Vec<Chunk>`. This is problematic in the spool model (it would require
rewriting bytes after commit) and violates separation of concerns.

In the streaming design, parsing and canonicalisation are separated:

**Parser** (pure, no side effects): validates CBOR structure, extracts
block metadata (types, flags, extents, CRC types), and detects
non-canonical encoding as a natural side effect — the decoded field
values differ from the wire bytes. Per-block canonicalisation flags
are returned alongside the parsed `Bundle` index and stored in
`BundleMetadata` so they are visible to early and late filters. The
parser does not rewrite or mutate the buffer. The `RewrittenBundle`
enum is eliminated — parse always returns a parsed result;
canonicalisation status is metadata, not a variant.

For rejected or malformed bundles, the BPA dispatcher may want to
send a status report back to the source (RFC 9171 §5.1). This
requires a **minimal parse** of just the primary block (source EID,
bundle ID, status report flags), which must succeed on any valid
primary block regardless of whether extension blocks or surrounding
structure are corrupt. The streaming parser does not provide this
itself — it's a separate cold-path concern owned by the dispatcher
(§10.3). `bpv7` exposes a per-field best-effort primary-block parse
(`BestEffortPrimaryBlock`) as a building block; the dispatcher calls
it on the accumulated bytes when the streaming parse fails, maps the
resulting error to a `status_report::ReasonCode`, and emits the
report. Bundles whose primary block is too corrupt for even the
best-effort parse to extract a source EID are silently dropped (no
report destination known).

**Canonicalisation filter** (late mutating filter): runs after the
original bundle is committed to storage. Consumes the parser's
per-block canonicalisation flags, re-encodes only flagged blocks from
the parsed `Bundle` field values (which are inherently canonical), and
produces a `Transformer` — the filter infrastructure performs a
generational rewrite (§6.6). Unflagged blocks are left untouched.
Policy-driven: some deployments may preserve the original encoding
(e.g., for forensic forwarding).

Well-behaved implementations produce canonical CBOR; the parser sets
no flags and the canonicalisation filter is a no-op. Non-canonical
blocks step off the hot path — the flagged blocks must be re-parsed
by the filter to extract field values. Because the original wire bytes
are committed before canonicalisation runs, a crash during
canonicalisation never loses a valid bundle. On restart, any committed
bundle that hadn't completed ingress processing is known to have
passed early filters; recovery resumes from the late mutating filters,
skipping the early filter gate.

#### 6.2.3. Header Segment Write Constraint

The first chunk pushed into `BundleStorage::store()` MUST contain
the **complete header segment** — primary block through all
extension blocks — at its start, as a single contiguous run of
bytes. This ensures header blocks are stored contiguously at the
start of the bundle data.

The first chunk MAY extend past the header segment into the payload.
The parser's `push()` returns the full concatenation of bytes it
consumed while reaching the payload-header boundary (`ParserProgress::Ready(Bytes)`);
whatever the parser had accumulated when it identified the payload
block is what flows into the spool's first chunk. For small bundles
that arrive in a single CLA chunk, the first (and only) spool chunk
is the entire bundle — header + full payload. For large bundles
where the payload arrives across many subsequent chunks, the first
spool chunk is the headers plus whatever payload prefix happened to
sit in the same accumulation buffer.

The contiguity guarantee is "headers contiguous at the start of
stored data," not "headers exactly fill the first chunk." This
matches what the parser naturally produces and avoids the BPA
having to slice the returned bytes at `header_len`.

Payload bytes that arrived after the parser completed continue to
stream as subsequent chunks. This means that during egress, the
Transformer (§7.1) receives header blocks first, allowing it to
capture header data before the payload arrives.

The `Chunk` model is **not needed at ingress** — it is internal to
the Editor's Transformer (§7.1). At ingress, the accumulation buffer
is mutated in place by pre-filters; the parser is pure and can be
tested independently.

### 6.3. Filter Classification

Filters are split into early and late hooks for both ingress and
originate paths, plus egress and deliver:

| Hook | When | Payload available? | Phase metadata |
|------|------|-------------------|----------------|
| `EarlyIngress` | After header parse, before payload | No | `&IngressMetadata` |
| `EarlyOriginate` | After builder/parse, before spool write | No | `&IngressMetadata` |
| `Ingress` (late) | After bundle fully stored | Yes (via storage) | `&IngressMetadata` |
| `Originate` (late) | After bundle fully stored | Yes (via storage) | `&IngressMetadata` |
| `Egress` | Bundle dequeued for forwarding | Yes (via storage) | `&EgressMetadata` |
| `Deliver` | Bundle delivered to local service | Yes (via storage) | — |

The originate-raw path (`local_dispatch_raw()`, bundles from services
via gRPC) runs the same parser → early filter → canonicalise → spool
pipeline as ingress, since service-provided bytes may be non-canonical.
Only BPA-built bundles (`Builder`) skip canonicalisation.

#### 6.3.1. Phase-Specific Metadata

Not all metadata needs to survive the bundle's full lifecycle.
Phase-specific metadata is created for a processing phase and dropped
when that phase completes:

- **`IngressMetadata`** — created at parse time, dropped after
  dispatch: per-block canonicalisation flags, ingress CLA identity and
  peer authentication status, accumulation buffer reference.
- **`BundleMetadata`** — carried for the bundle's full lifecycle in
  storage: status, storage_name, received_at, flow label (set by
  early filters), routing state.
- **`EgressMetadata`** — created when the bundle is dequeued for
  forwarding, dropped after CLA send completes or fails: target peer,
  CLA address, queue assignment, egress filter decisions.

All filters receive `&BundleMetadata` (immutable). Write filters that
need to update metadata send a `FilterOut::Metadata` replacement
through their output stream (§6.3.3). This prevents `BundleMetadata`
from becoming a dumping ground for phase-specific state.

#### 6.3.2. Filter Behaviour

**ReadFilters** make a filtering decision only — accept or reject.
They cannot mutate bundle data or metadata.

**WriteFilters** can mutate both data and metadata, sending updates
through their `&dyn Sender<FilterOut>` output.

The early/late distinction controls what bytes filters receive, not
which trait they use:

- **Early filters** (before payload arrival) receive a
  `Receiver<Bytes>` that yields the accumulation buffer as a single
  `Bytes` then `None` — all header blocks in one chunk; payload is
  not available.
  - *Early ReadFilter*: inspects headers, accepts/rejects (e.g.,
    bundle validity, BPSec header-block BIB verification via Verifier
    §7.1.2).
  - *Early WriteFilter*: inspects headers, can update metadata (e.g.,
    set `flow_label` / priority) or configure inline transforms on the
    payload stream (§6.5).

  Early filters cannot inspect payload content. This is critical for
  space DTN durability: if early filters accept, the original wire
  bytes are committed to storage before any data mutation occurs. A
  valid bundle is never lost because a mutation failed mid-stream.

- **Late filters** (after bundle fully stored) receive a
  `Receiver<Bytes>` backed by `BundleStorage::load()`:
  - *Late ReadFilter*: inspects payload content, accepts/rejects.
    Late read filters at the same level run in parallel via
    `Bytes::clone()` fan-out (§6.3.3).
  - *Late WriteFilter*: rewrites the bundle via generational save
    (§6.6); can also update metadata via `FilterOut::Metadata`.

**Canonicalisation** is a late write filter — runs after commit as a
generational rewrite. If it fails, the original non-canonical bundle
is safely stored.

BPSec integrity and confidentiality are implemented as built-in
filters, not as separate Signer/Encryptor/Verifier types. The filter
implementations use the Editor and BPSec crypto primitives (§7.1.6)
internally.

#### 6.3.3. Filter API

The current filter API receives `(Bundle, Bytes)` and returns
`(Bundle, Bytes)`. This does not work in the streaming model because
the full `Bytes` may not be in memory. The streaming API uses the
stream traits from §5 directly: `Receiver<Bytes>` for input,
`Sender<FilterOut>` for write-filter output:

```rust
enum FilterResult {
    Continue,
    Drop(Option<ReasonCode>),
}

enum FilterOut {
    Data(Bytes),
    Bundle(Bundle),
    Metadata(BundleMetadata),
}

#[async_trait]
trait ReadFilter: Send + Sync {
    async fn filter(
        &self,
        bundle: &Bundle,
        metadata: &BundleMetadata,
        input: &dyn Receiver<Bytes>,
    ) -> Result<FilterResult>;
}

#[async_trait]
trait WriteFilter: Send + Sync {
    async fn filter(
        &self,
        bundle: &Bundle,
        metadata: &BundleMetadata,
        input: &dyn Receiver<Bytes>,
        output: &dyn Sender<FilterOut>,
    ) -> Result<FilterResult>;
}
```

Both traits share `FilterResult` (accept or reject) and receive
`&BundleMetadata` (immutable). The `Bundle` is the block index —
offsets, types, flags. The filter uses the index to navigate the
byte stream and slices into it to read block content.

**ReadFilter** receives an input `Receiver<Bytes>` only. It pulls
chunks via `input.recv().await` until `None`, inspecting block
content, and returns accept/reject.

**WriteFilter** receives both input and output. It reads from `input`,
transforms, and pushes results through `output.send(...)`:

- `FilterOut::Bundle(Bundle)` — updated block index, sent early so
  chained write filters can begin planning before byte streaming
  completes
- `FilterOut::Metadata(BundleMetadata)` — updated metadata, sent as a
  full replacement (last one wins)
- `FilterOut::Data(Bytes)` — transformed bundle bytes

The filter sends `Bundle` and optionally `Metadata` through the
output, then streams `Data`. No mutable borrow on `BundleMetadata`
is needed; updates are sent as copies. The filter uses the Editor
and Transformer internally — that is a private implementation detail.

**Byte source varies by phase:**

- Early filters: BPA wraps the accumulation buffer in a
  `Receiver<Bytes>` that yields a single chunk then `None`.
- Late filters: BPA spawns a `BundleStorage::load()` task pushing into
  a channel, hands the receiver-side `Receiver<Bytes>` to the filter.
- Egress filters: same as late — streamed from storage.

The filter does not know or care which adapter backs the
`Receiver<Bytes>`.

**Parallel read filters** at the same dependency level each receive
their own `&dyn Receiver<Bytes>`. The BPA reads from storage once
and fans out via `Bytes::clone()` (refcount bump, not a data copy)
into each filter's bounded channel. Backpressure propagates naturally.

```
load()
  |
  fan-out (Bytes::clone per filter)
  |         |         |
  v         v         v
ReadFilter ReadFilter ReadFilter
  (bounded)  (bounded)  (bounded)
```

**Write filters** run sequentially. The BPA wires the streams for a
generational rewrite (§6.6):

```
load() → Receiver<Bytes> → WriteFilter
                                |
                                v
                    Sender<FilterOut> → BPA routes:
                        Bundle   → next filter's index
                        Metadata → metadata store
                        Data     → next filter's Receiver<Bytes>
                                   (or store() on the last filter)
```

**Write filter chaining** pipelines through the stream traits: the BPA
extracts `Data` variants from each filter's `Sender<FilterOut>` and
exposes them as the next filter's `Receiver<Bytes>`, while routing
`Bundle` and `Metadata` to the BPA's own state. The `Bundle` index is
sent through the channel early — the next filter can begin planning
before the previous filter finishes streaming. Only the final filter's
output is spooled to storage as a single generational rewrite.

### 6.4. Early Filter Gate

After all header blocks are parsed and before the payload arrives, the
early filter gate runs.

If a pre-filter **rejects**: the BPA returns from `Sink::write()`
without commencing a spool. The CLA's pushing task sees `SendError`
on its next push and can cancel the transfer (e.g., TCPCLv4
XFER_REFUSE, UDPCLv2 stops accepting datagrams). For a 1GB payload
from a rejected source, the BPA has received only the header blocks.
Zero wasted I/O.

This is effectively **DDoS protection**. Without early filtering, a
DTN node is trivially DoS-able: an attacker sends oversized bundles
with forged sources, and the victim must receive, parse, store, and
process the entire payload before deciding to reject. With early
filtering, the BPA inspects headers (~hundreds of bytes), rejects, and
the CLA refuses the transfer mid-stream. The attacker pays for a few
KB of headers; the victim pays nothing for the payload. This is
critical for space DTN links where bandwidth is extremely scarce.

If a pre-filter **accepts**: open a spool via `BundleStorage::store()`,
push the accumulated header bytes as the first chunk, then forward
subsequent CLA chunks through any configured transforms into the
spool channel.

### 6.5. Inline Payload Transforms and Durability

A pre-filter may configure a transform on the payload stream. The
primary use case is **security gateway payload decryption**.

Ingress payload transforms use Transformers (§7.1) — the same
push-based model used for egress. CRC verification uses a Verifier
(§7.1.2) — the same push-based consumer model. These compose
sequentially:

```
CLA chunks -> [CRC Verifier] -> [BPSec decrypt Transformer] -> spool channel
```

**AES-GCM streaming decryption**: AES-GCM uses CTR mode internally and
can decrypt chunk by chunk. Authentication tag verification is
deferred until the final chunk. The spool must not be committed until
tag verification succeeds; on failure the spool task is cancelled via
its token and discards the staged data.

**Durability.** In space DTN scenarios, bundle data is extremely
precious. Once the CLA receives the last byte, the BPA must not lose
it.

- *Normal case (no payload transform)*: the spool task writes through
  to a temp file as data arrives. Every byte is on disk as it's
  written (sequential append). When the producer channel closes,
  `store()` performs `fsync` + rename and returns. The bundle is
  durable.
- *Transform case (payload decryption)*: the spool contains decrypted
  data. The producer side withholds channel close until tag
  verification. If the BPA crashes before verification, the decrypted
  spool is discarded on recovery (temp file, no metadata pointing to
  it). The original encrypted bundle must be retransmitted. This is
  inherent — you cannot commit unverified data.

### 6.6. Generational Rewrites

Bundle data has two generations during ingress: the original wire
bytes, and the final output after all late write filters.

```
Generation 0: original wire-format bytes from CLA
  committed during Phase B (spool from CLA, §6.7)

  → load(gen0) → Receiver<Bytes>
  → WriteFilter 1 (Receiver<Bytes> → Sender<FilterOut>)
  → WriteFilter 2 (Receiver<Bytes> → Sender<FilterOut>)
  → ...
  → store() → new spool

Generation 1: final output after all late write filters
  committed (fsync + rename)
  metadata.update(storage_name = gen1)
  delete(gen0)
```

Late write filters chain through the stream traits — each filter's
`Sender<FilterOut>` output `Data` variants become the next filter's
`Receiver<Bytes>` input (§6.3.3). No intermediate spools between
filters. Only the final output is committed as a new generation.

**Crash recovery**: the metadata's `storage_name` always points to the
last successfully committed generation. If the BPA crashes during
write filter processing, the in-flight `store()` task's temp file has
no metadata reference — cleaned up on recovery. The last committed
generation is intact. Recovery reruns the write filters for the
current processing phase from the last committed generation.

If no late write filters mutate (or none are registered), generation 0
is the final generation — no rewrite occurs.

### 6.7. Complete Ingress Flow

```
CLA wire
  | transfer segments (or complete Bytes via dispatch())
  v
sink.write(stream: &dyn Receiver<Bytes>, ...)
  | CLA pushes chunks into the channel feeding the Receiver
  | CLA closes the channel at end of bundle
  v
Ingest
  |-- Phase A:
  |     accumulate + parse primary block
  |     accumulate + parse extension blocks
  |     (non-canonical blocks flagged, not rewritten)
  |
  |-- Early filter gate (EarlyIngress) — read-only
  |     includes BPSec header verification
  |     sets flow_label / priority on BundleMetadata
  |
  |     REJECT --> return from Sink::write()
  |               (zero I/O, no storage, no metadata)
  |
  |     ACCEPT --> spawn BundleStorage::store(spool_stream, cancel)
  |               push complete header segment as first chunk
  |               MetadataStorage::store()
  |               start routing lookup (async, needs only metadata)
  |
  |-- Phase B (tee'd):
  |     payload chunks pulled from the CLA stream are tee'd:
  |       ├→ spool_sender.send(chunk)  [feeds store() task]
  |       └→ late read filters         [Bytes::clone fan-out]
  |            (payload inspection, BIB verification,
  |             ingress-time HMAC computation)
  |
  |-- on CLA stream closed (end of bundle):
  |     drop spool_sender → store() returns storage_name (fsync + rename)
  |     Late write filter gate (Ingress) — mutating
  |       canonicalise flagged blocks (if policy, generational rewrite)
  |       other late filter mutations (generational rewrite)
  '--   enqueue(dispatch, priority from early filter)
        routing result available (started during Phase B)
```

Routing lookup begins as soon as early filters accept and metadata is
stored — it needs only `BundleMetadata` (destination, priority), not
payload data. The lookup runs concurrently with payload spooling and
late read filters. By the time late write filters complete and the
bundle is ready for dispatch, the routing result is typically already
available.

Late read filters that reject during spooling cause the spool task to
be cancelled (token signalled). This wastes some I/O but is necessary:
the BPA has accepted custody from the CLA after early filters pass, so
spooling must begin immediately. A late read filter rejection is a
policy decision to drop a bundle the BPA already owns.

## 7. Egress: Storage to CLA

### 7.1. The Transformer Model

The egress path is driven by **Transformers** — push-based streaming
processors that consume stored bundle bytes sequentially and emit
transformed bytes. Transformers are produced by the Editor and filter
implementations during a planning phase that inspects the `Bundle`
block index, then executed by the BPA pushing stored bytes through.

#### 7.1.1. Transformer Interface

```rust
type Transformer = Box<dyn FnMut(Option<Bytes>) -> Result<TransformResult>>;

pub enum TransformResult {
    /// Not enough input to produce output yet.
    NeedMore,
    /// Emit these byte segments. Vec because a single push may
    /// cross block boundaries — e.g., a new header block followed
    /// by a passed-through extension block — and concatenating
    /// them would be wasteful.
    Emit(Vec<Bytes>),
    /// Final output + the updated block index. Only returned in
    /// response to None (end of input).
    Done(Vec<Bytes>, Bundle),
}
```

The calling contract:

- `Some(bytes)` — push input data. Returns `NeedMore` or
  `Emit(Vec<Bytes>)`.
- `None` — signal end of input. The Transformer flushes any buffered
  state and returns `Done(Vec<Bytes>, Bundle)`.
- `Err` at any point — the input stream is invalid; the caller
  abandons the stream and the spool (if applicable).
- The Transformer always consumes all input. `Done` only appears in
  response to `None`. A Transformer that detects a problem mid-stream
  returns `Err`, not `Done`.

The `Done` variant returns the updated `Bundle` block index,
reflecting any blocks that were added, removed, or modified.

**Calling pattern (egress executor):**

```rust
let (load_tx, load_rx) = bounded(N);
spawn(storage.load(&storage_name, &ChannelSender(load_tx)));
let load_stream = ChannelReceiver(load_rx);

let (cla_tx, cla_rx) = bounded(N);
let cla_handle = spawn(cla.write(queue, addr, &ChannelReceiver(cla_rx), total_len));

while let Some(chunk) = load_stream.recv().await {
    match transformer(Some(chunk))? {
        NeedMore => continue,
        Emit(parts) => for p in parts { cla_tx.send(p).await? },
        Done(..) => unreachable!(),
    }
}
match transformer(None)? {
    Done(parts, bundle) => {
        for p in parts { cla_tx.send(p).await? }
        drop(cla_tx);                       // signal end of bundle
        let result = cla_handle.await??;    // ForwardBundleResult
        // bundle = updated block index
    }
    _ => unreachable!(),
}
```

#### 7.1.2. Verifier: Push-Based Consumer

The Verifier follows the same push-based input model but is a
**consumer**, not a transform — it validates data without producing
output bytes.

```rust
type Verifier = Box<dyn FnMut(Option<Bytes>) -> VerifyResult>;

pub enum VerifyResult {
    NeedMore,
    Done(Result<()>),
}
```

Same `Option<Bytes>` input contract. The Verifier captures primary
block fields and target block content as bytes flow through, then
computes and checks signatures. For header-block BIBs, verification
completes as soon as the target block has passed. For payload-block
BIBs, the Verifier incrementally computes HMAC over the payload and
verifies on `None`.

Use cases:

- **Early ingress**: Verifier pushed the accumulation buffer,
  validates header-block BIBs against in-memory header data.
- **Payload BIB**: Verifier runs alongside ingress spooling, receiving
  the same bytes that flow to storage.
- **Filters**: a filter can run a Verifier alongside a Transformer,
  feeding the same input to both. The Verifier validates while the
  Transformer transforms; the filter infrastructure checks the
  Verifier's result before committing.

#### 7.1.3. Editor Produces a Transformer

The Editor plans against the concrete `Bundle` struct — block numbers,
types, flags, byte extents. The existing `Vec<Chunk>`
(`Unchanged(Range)` / `New(Box<[u8]>)`) becomes **internal state** of
the Transformer closure, not an exposed data structure. The
Transformer is a state machine that:

1. Tracks byte position in the input stream
2. At each block boundary, decides: pass through (unchanged block),
   substitute (new header bytes, skip original), or inject (emit new
   block bytes at this position)
3. Emits output bytes as they become available
4. On `None`, returns the updated `Bundle` index

The planning phase does not need `source_data` — it only inspects the
block index. The Transformer receives the source bytes at execution
time. The `'a` lifetime is removed from the Editor; `Cow<'a, [u8]>`
in `BlockTemplate` is replaced with owned bytes for caller-provided
data and positional references for existing block data (resolved by
the Transformer as bytes flow through).

#### 7.1.4. Chained Transformers

Transformers compose by encapsulation. An outer filter wraps an inner
filter's Transformer, processing its output:

```
Confidentiality filter
  └─ Integrity filter
       └─ Editor's Transformer
            └─ (receives stored bytes)
```

Each stage has full structural knowledge at planning time:

- **Editor** plans against the original `Bundle` index, produces a
  Transformer and an updated `Bundle` index reflecting its
  modifications.
- **Integrity filter** uses the Editor to insert a BIB block, then
  wraps the Editor's Transformer. The wrapping Transformer:
  - Pushes input bytes into the Editor's Transformer
  - Receives the Editor's `Emit` outputs
  - Captures IPPT fields (primary block data) as they flow past
  - Buffers BIB target blocks, computes HMAC incrementally
  - Injects BIB blocks at the correct position
  - Emits its own output
- **Confidentiality filter** wraps similarly — captures AAD from the
  primary block, buffers BCB target blocks, encrypts, and injects BCB
  blocks.

The `Signer` and `Encryptor` as standalone public types are
**eliminated**. Their orchestration logic dissolves into the integrity
and confidentiality filter implementations. The Editor remains as a
general-purpose library component; BPSec crypto primitives remain as
reusable low-level APIs (§7.1.6).

The outermost Transformer is the only thing the executor touches.
Stored bytes flow in, CLA-ready bytes flow out. The layered planning
and execution is invisible to the caller.

#### 7.1.5. Streaming Payload Crypto

Header-target crypto (BIB/BCB on extension blocks) is handled
naturally by the Transformer chain — header blocks are small and
captured in full as they flow through.

Payload-target crypto has a **wire-format ordering constraint**: the
BIB/BCB is an extension block that must appear before the payload
block (RFC 9171 requires payload last), but its content (the HMAC
digest or authentication tag) can only be computed after reading the
entire payload. This is inherent to BPv7.

**Payload BIB (HMAC) — two-pass at egress.** If an integrity filter
adds a payload BIB at egress, the executor must read the stored bundle
twice:

1. *First pass*: stream the payload through the Transformer to compute
   the HMAC incrementally (`mac.update()` is push-ready). The
   Transformer accumulates IPPT header fields and the HMAC digest but
   emits no output.
2. *Second pass*: the Transformer now has the HMAC result. It emits
   header blocks, the BIB (with HMAC value), then passes through the
   payload to the CLA.

For local disk / NOR flash, the second read is essentially free (OS
page cache). For S3, it is a second full GET — but this case is
narrow (security gateway adding payload BIB at egress).

**Payload BIB — preferred: compute at ingress.** The cleaner approach
is to compute the payload BIB at **ingress time**, when the payload
bytes are already streaming past:

- The security gateway's ingress policy identifies bundles that need a
  payload BIB.
- A Verifier-like consumer runs alongside payload spooling, computing
  the HMAC incrementally as bytes flow to storage.
- The BIB is added as a late ingress filter (generational rewrite)
  after the HMAC is complete.
- At egress, the BIB is already stored — no extra work.

This avoids the two-pass problem entirely. The HMAC is computed once,
during the single ingress pass, and durably stored.

**Payload BCB (AES-GCM)** has the same ordering constraint but is
deferred to Phase 3 (security gateway). AES-GCM requires a streaming
wrapper built on the low-level `aes` + `ghash` crates (§8.3).

#### 7.1.6. BPSec Low-Level API Surface

The BPSec crypto primitives are reusable building blocks for filter
implementations. They remain a low-level library, currently in
`bpv7/src/bpsec/` (moving to `hardy-bundle` if and when that crate
split lands — see §10.1):

**IPPT/AAD construction** — the core of both signing and encryption.
Constructs the integrity or authentication input from scope flags,
primary block bytes, and target/security block header fields. The
construction is a sequence of incremental updates:

```
scope_flags → [primary block bytes] → [target header fields]
  → [security header fields] → target payload bytes
```

Each step is a `mac.update()` or AAD accumulation call. The filter
provides these pieces as they flow through the Transformer — primary
block captured early, target block header fields from the `Bundle`
index, payload bytes streamed incrementally.

**Crypto operations:**

- `bib_hmac_sha2` — HMAC computation. Already incremental (`hmac`
  crate's `mac.update()`). Push-ready for Transformers.
- `bcb_aes_gcm` — AES-GCM encryption/decryption. Currently requires
  contiguous buffer (`aes-gcm` crate). Streaming wrapper deferred to
  Phase 3.

**Key management** — `KeySource` trait, `Key` struct, AES key
wrapping. Already clean and filter-agnostic.

**Operation result types** — `Parameters`, `Results`, `OperationSet`.
These are the CBOR serialization format for BIB/BCB block payloads.
Filters use them to encode the BIB/BCB data that the Editor inserts.

**What is removed:**

- `BlockSet` trait — replaced by the Transformer's internal state
  capturing block data as it streams past
- `Signer` struct — orchestration dissolves into integrity filter
- `Encryptor` struct — orchestration dissolves into confidentiality
  filter
- `EditorBlockSet` — no longer needed without `BlockSet`

### 7.2. CLA Egress: Cla::forward and Cla::write

The existing `Cla::forward(Bytes)` method is retained for CLAs that
expect a complete bundle in memory. A new streaming variant takes a
`Receiver<Bytes>` from which the CLA pulls chunks, mirroring
`Sink::write` from §6.1:

```rust
trait Cla {
    // Existing — full bundle in memory
    async fn forward(
        &self, queue: Option<u32>,
        cla_addr: &ClaAddress,
        data: Bytes,
    ) -> Result<ForwardBundleResult>;

    // New — streaming forward. BPA passes a Receiver<Bytes>; CLA
    // pulls chunks until the stream closes, then returns the
    // forward result.
    async fn write(
        &self, queue: Option<u32>,
        cla_addr: &ClaAddress,
        stream: &dyn Receiver<Bytes>,
        total_len: u64,
    ) -> Result<ForwardBundleResult>;
}
```

CLAs that support streaming implement `write()` directly and
`forward()` via a small adapter that wraps the input `Bytes` as a
single-item `Receiver`. For non-streaming CLAs, the BPA wraps them
in an adapter that collects `recv()` calls into a contiguous `Bytes`
and calls `forward()`.

Internally, the BPA always uses `write()`. The egress executor reads
sequentially from storage, pushes bytes through the Transformer chain,
and feeds the Transformer's output into the channel backing the CLA's
`Receiver`. For streaming CLAs, bytes flow directly to the wire; for
adapted non-streaming CLAs, bytes are collected and forwarded as a
single `Bytes` once the stream closes.

`total_len` can be computed from the Transformer's plan knowledge
(original bundle size adjusted for block additions/removals). Needed
by CLAs that must frame the transfer (e.g., TCPCLv4 XFER_SEGMENT
length). Migration to streaming-only is transitional (§6.1).

### 7.3. The Common Forward Path

For the hot forward path (no BPSec added at this node, no egress
filter mutations), the Transformer is a simple state machine. It
identifies blocks by `block_type` in the `Bundle` index (§10.3 —
the Bundle does not carry decoded extension-block fields), decodes
the body from the streamed bytes as they flow past, mutates, and
re-emits:

1. Receive primary block bytes → emit rewritten primary (~50B)
2. Receive previous_node block → decode body, update, emit (~30B)
3. Receive hop_count block → decode body, update, emit (~15B)
4. Receive bundle_age block → decode body, update, emit (~15B)
5. Receive remaining extension blocks → pass through unchanged
6. Receive payload → pass through unchanged

No crypto. No random access. Sequential read from storage, through
the Transformer, to the CLA. Peak memory: the read buffer plus ~110
bytes of new header blocks.

### 7.4. Read-Only Storage on the Forward Path

The Transformer's output goes directly to the CLA — it is never
written back to storage. The original bundle data remains untouched
until `delete()`.

This means:

- **Header segment growth is not a problem.** The Editor may add
  blocks, integrity filters may insert BIBs — but the output streams
  to the CLA, not back to storage.
- **No generational rewrite for forwarding.** Generational saves
  (§6.6) are only for late filter mutations. The forward path is
  read-once, stream through Transformer, delete.

### 7.5. Failure Handling

If CLA transmission fails, `Cla::write()` returns `Err`; the BPA
cancels the producer task feeding its `Receiver`. The original bundle
data is still on disk. The bundle stays in its queue for retry. The
next attempt constructs a fresh Transformer and channel pair and
re-applies egress mutations from scratch.

## 8. Bundle Struct Reference

### 8.1. Bundle and BundleMetadata

The `Bundle` struct (introduced in §2.3) is a concrete block index:

```rust
pub struct Bundle {
    pub blocks: HashMap<u64, Block>,
}
```

Each `Block` records structural metadata only: byte extent in the
wire data (`Range<u64>`), block type, flags, CRC type, BPSec
coverage state (BIB/BCB references), and data range within the block
extent. `u64` (rather than `usize`) is used so offsets remain valid
on 32-bit targets where bundle storage may exceed `usize::MAX`. There is no `dyn Bundle` trait, no `BlockSet` trait, and no
multiple implementations — a single concrete representation used
everywhere (parser output, Editor input, Transformer output).

`BundleMetadata` carries BPA-internal state: status, storage_name,
received_at, flow_label, routing state, and decoded primary block
fields needed for indexed queries (destination for routing, source for
identity, lifetime for expiry). The BPA extracts these during ingress
from the accumulation buffer, storing them in `BundleMetadata`. After
ingress, the BPA pipeline operates on `BundleMetadata` for routing,
queuing, and status decisions; the `Bundle` index is only needed when
constructing a Transformer for egress or applying a late filter.

Clean separation: `bpv7` owns structure, `bpa` owns semantics.

### 8.2. Parser Output

The parser returns the concrete `Bundle` index plus per-block
canonicalisation flags (§6.2.2). It validates CBOR structure, records
block extents, and detects non-canonical encoding. It does not decode
block content beyond what is needed for structural validation. The
`RewrittenBundle` enum is eliminated, as is the broader
`Checked`/`Rewritten`/`Parsed` taxonomy — see §10.3 for the full
collapse of parse modes into streaming primitives + a single
in-memory sugar function.

### 8.3. Incremental Crypto Readiness

| Component | Crate | Already Incremental | Streaming Difficulty |
|-----------|-------|--------------------|----|
| CRC-16/32 | `crc` v3 | Yes (`digest.update()`) | **Low** — calling convention change |
| BIB HMAC-SHA2 | `hmac` v0.13 | Yes (`mac.update()`) | **Low** — initialise with headers, push payload |
| BCB AES-GCM | `aes-gcm` v0.10 | No (contiguous only) | **High** — need streaming wrapper or crate swap |

AES-GCM is AES-CTR + GHASH, both inherently streamable. A streaming
wrapper built on the low-level `aes` + `ghash` crates is feasible but
deferred to the security gateway phase. The Transformer model makes
this straightforward — the confidentiality filter's Transformer
processes payload bytes incrementally as they flow through.

## 9. Storage Segmentation and Caching

### 9.1. Headers vs Payload

Bundle data is logically segmented:

- **Header segment**: primary block + all extension blocks (including
  BIB/BCB). Typically a few hundred bytes to a few KB. Always needed
  for block-level operations.
- **Payload segment**: payload block data. Variable size, potentially
  very large. Only needed for delivery, crypto target processing, or
  verbatim forwarding.

### 9.2. Cache Strategy

With sequential-only storage access, the cache simplifies back to what
Hardy already has: an **LRU cache of small bundles**. No layout
awareness, no header segment extraction, no split caching strategy.

| Bundle size | Cache strategy |
|------------|----------------|
| Small (< threshold) | LRU cache, take() on load (single refcount, `try_into_mut()`) |
| Large (> threshold) | Not cached; stream from backend on each access |

The cache is populated only on `store()` / `replace()` — never on
`load()`. Load takes from the cache (single refcount for in-place
mutation). This write-on-store, take-on-load model means the cache
acts as a single-use buffer bridging the `store()` → `load()`
handoff.

No header segment caching is needed — the Transformer model does not
require random access to headers. Headers flow through the
Transformer sequentially, captured as needed.

### 9.3. Backend Considerations

| Backend | Sequential I/O | Notes |
|---------|---------------|-------|
| Local disk | Natural (read/write syscalls) | Optimal for append + sequential scan |
| SSD | Natural | No seek penalty regardless |
| NOR flash (space) | Natural | Sequential read is the native primitive |
| S3 / object store | `PUT` / `GET` | Single request per operation, no range requests needed |

Every backend handles the trait natively without adaptation layers.

## 10. Crate Structure

### 10.1. Crate Responsibilities

**`hardy-async`** — channel and stream trait primitives:

- `channel::Sender` / `channel::Receiver` (bounded channels)
- `Sender<T>` / `Receiver<T>` traits and channel adapters
  (`ChannelSender<T>`, `ChannelReceiver<T>`)

**`bpv7`** — wire format, structural indexing, type definitions:

- CBOR encoding/decoding (`FromCbor`, `ToCbor`)
- Block structures, CRC, EID, bundle types (`Block`, `Flags`, `Id`)
- `Bundle` struct — concrete block index (`HashMap<u64, Block>`)
- Parser — wire bytes → `Bundle` index + canonicalisation flags

**`hardy-bundle`** (optional split, deferred) — bundle manipulation
and Transformer production:

- `Transformer` type and `TransformResult` enum
- `Verifier` type and `VerifyResult` enum
- `Editor` — plans against `Bundle` index, produces `Transformer`
- `Chunk` — internal to Editor's Transformer (not exposed)
- BPSec low-level crypto APIs (IPPT/AAD construction, HMAC, AES-GCM,
  key management, operation result types)

The `Signer` and `Encryptor` structs are eliminated; their
orchestration logic moves into BPSec filter implementations in the
`bpa` crate. The Editor and crypto primitives remain as reusable
library components.

Whether to split `hardy-bundle` from `bpv7` is deferred until the
Transformer interfaces stabilise. The Editor is tightly coupled to
`bpv7` types; the split becomes a straightforward refactor once the
boundary is clear.

**`bpa`** — infrastructure and execution:

- Egress executor — spawns `BundleStorage::load()`, drives the
  Transformer chain, feeds `Cla::write()`'s `Receiver`
- Storage traits and backends (`store()` / `load()` / `delete()`)
- Cache (small bundle caching, take semantics)
- Dispatcher, routing, queues, reaper
- CLA/service registries; `Sink::write()` / `Cla::write()` surfaces
- `BundleMetadata` — BPA-internal state (status, flow_label, decoded
  primary block fields, etc.)
- Filter trait and infrastructure (uses `Receiver<Bytes>` for input,
  `Sender<FilterOut>` for write-filter output)
- Built-in BPSec filters (integrity, confidentiality) — use Editor +
  crypto primitives to produce Transformers and Verifiers
- Generational rewrite executor (loads stored bundle, pipes through
  write filter chain into a new `store()` call)
- `GenerationGuard` — RAII spool task wrapper with cancellation (§4.3)

### 10.2. Dependency Graph

```
hardy-async ← bpv7 ← [hardy-bundle] ← bpa
(Sender,    (wire    (Editor,         (Filter trait,
 Receiver,    types,   Transformer,    BPSec filters,
 channels)     Bundle   Verifier,       BundleMetadata,
               index,   BPSec crypto    egress executor,
               Parser)  primitives)     storage,
                                        cache,
                                        Sink::write,
                                        Cla::write)
                            ↑                ↑
                        Services          CLAs
                     (Builder, Editor) (transport only)
```

- `hardy-bundle` (if split) depends on `bpv7` for wire types and
  `hardy-async` for stream traits, not on `bpa`.
- **Services** depend on `hardy-bundle` (Builder, Editor, types) and
  `bpa` (Service trait).
- **CLAs** depend on `bpa` (Cla trait, Sink) and `hardy-async`
  (Receiver for byte streaming) but not on `hardy-bundle` — they are
  pure transport, delivering wire bytes to the BPA.
- **BPSec filters** (in `bpa`) use `hardy-bundle` for the Editor and
  crypto primitives. They are built-in filter implementations, not
  separate public APIs.

### 10.3. Library/Application Responsibility Split

The crate boundary follows a single principle: **`bpv7` owns
wire-format truth; `bpa` owns operational meaning.** `bpv7` exposes
the smallest set of building blocks that lets every consumer assemble
what it needs. Consumer-specific helpers — anything that bakes in
*how* a particular caller will interpret, route, or report on wire
data — live at the call site, not in the library.

This is more restrictive than the current architecture, which
embeds dispatcher-flavoured behaviour (e.g., `RewrittenBundle`'s
inline error → `ReasonCode` mapping, the three parse modes, decoded
extension-block fields on `Bundle`) directly in `bpv7`. The
streaming refactor unwinds that entanglement.

**Concrete consequences:**

- **Status report extraction is BPA-internal.** When the streaming
  parser fails, the BPA dispatcher may want to attempt a best-effort
  parse of the primary block to send a status report back to the
  source (RFC 9171 §5.1). The extraction itself is performed by the
  BPA, calling into a `pub` `bpv7::bundle::primary_block::BestEffortPrimaryBlock`
  building block (the existing per-field `Result<T, Error>`
  intermediate type, refactored from today's `primary_block::PrimaryBlock`).
  No `extract_for_status_report` function exists in `bpv7`'s public
  API — that helper, with its dispatcher-specific reason-code
  mapping, lives in `bpa::dispatcher`.

- **Reason-code mapping is BPA policy.** The translation from a
  `bpv7::Error` variant to a `status_report::ReasonCode`
  (`BlockUnsupported`, `BlockUnintelligible`, etc.) is a policy
  decision about how to interpret a parse failure for reporting
  purposes. It lives at the BPA call site, not in `bpv7`. The
  `status_report::ReasonCode` enum stays in `bpv7` (it's part of the
  wire format for status report payloads), but the *mapping logic*
  is the dispatcher's.

- **Decoded extension-block fields leave `Bundle`.** Today's
  `Bundle` carries `previous_node`, `age`, `hop_count` decoded from
  their respective extension blocks at parse time. After the
  refactor, `Bundle` is the structural index only (§8.1). Filters
  that need `previous_node` for loop detection or `hop_count` for
  forwarding decisions decode the body on demand from the block's
  byte extent, using `bpv7`'s low-level CBOR primitives. Adding a
  new extension block type stops being a `bpv7` change.

- **The `Checked`/`Rewritten`/`Parsed` taxonomy collapses.** The
  three parse modes exist today to express dispatcher decisions
  (canonicalise? drop unsupported blocks? validate BPSec?) as
  parser variants. Once those decisions move to filter chain
  configuration, the taxonomy is redundant. `bpv7`'s in-memory
  parse surface collapses to a single sugar function over the
  streaming primitives, returning a fully-decoded bundle for tools,
  tests, and builder round-trips. Production callers (the BPA) use
  the streaming parser directly.

- **Reusable filter logic lives in `bpa`, not `bpv7`.** Common
  policy filters (hop count, bundle age, previous-node mutation)
  are filter implementations against `bpv7`'s lean `Bundle` index.
  They depend on `bpv7` for wire-format primitives but are not
  themselves `bpv7` types. Bundling them with the BPA (or in a
  shared `bpa::filters::common` module) keeps `bpv7` consumable by
  non-Hardy callers (CLA debug tools, external utilities) without
  dragging dispatcher policy along.

**The acceptance test** for whether something belongs in `bpv7` or
`bpa`: *if BPA needs to query, route, or report by it, it's
operational meaning and lives in `bpa`; if it's purely structural
(byte extents, CBOR shapes, RFC-defined field decodings), it lives
in `bpv7`.* The cold-path status report helper fails this test —
it's BPA's specific need, even though it composes `bpv7` building
blocks.

**`bpv7`'s public parse surface** after the refactor is roughly:

```rust
// Streaming primitives (hot path; BPA + tests)
pub use streaming::{BundleParser, ParserProgress};

// In-memory sugar (tools, tests, builder round-trips)
pub fn parse_bundle(data: &[u8]) -> Result<FullBundle, Error>;

// Best-effort primary-block parse (BPA cold path composes this)
pub use bundle::primary_block::BestEffortPrimaryBlock;
```

Three entry points, each with a one-sentence purpose. The
dispatcher's status-report helper, the reason-code mapping, the
filter implementations, and the operational policy that wraps these
primitives all live in `bpa`.

## 11. Implementation Phasing

### Phase 0: Transformer Prototype

1. Define `Transformer` type, `TransformResult` enum, `Verifier`
   type, `VerifyResult` enum
2. Simplify `Bundle` struct to block index only — remove decoded
   primary block fields and extension block values
3. Move decoded field extraction to BPA-side helpers (destination,
   source, hop count, etc. decoded from accumulation buffer at
   ingress, stored in `BundleMetadata`)
4. Refactor Editor to plan against `Bundle` index, produce a
   `Transformer` closure. `Vec<Chunk>` becomes internal to the
   closure. Remove `source_data` from `Editor::new()` and
   `rebuild()` — source bytes arrive via Transformer push.
5. Remove `'a` lifetime from Editor
6. Retain `flatten()` / `flatten_inplace()` for tests and backwards
   compatibility during transition

**Adjust blocks and CRC.** The `BlockTemplate::Adjust` variant —
"same payload, different metadata" — is naturally handled by the
Transformer model. When flags or CRC type change but payload does
not, the Transformer emits the re-encoded header bytes (`New`), then
passes through the original payload bytes as they arrive. CRC is
computed incrementally over the new header + streamed payload using
`crc` crate's `digest.update()`. No contiguous buffer is required.
No `source_data` parameter is needed.

### Phase 1: Stream Trait Foundations

1. Define `Receiver<T>` trait alongside the existing `Sender<T>`
   in `hardy-async`
2. Provide `ChannelReceiver<T>` adapter (mirror of
   `ChannelSender<T>`)
3. Document the direction conventions (`Sender` parameter = method
   pushes; `Receiver` parameter = method pulls) in `hardy-async`
   rustdoc

### Phase 2: Streaming Storage and Egress

1. Replace `BundleStorage::save()` / `load()` / `read_at()` with
   `store(&dyn Receiver<Bytes>, CancellationToken)` /
   `load(&str, &dyn Sender<Bytes>)` / `delete(&str)`
2. Update each backend (localdisk, S3, in-memory, sqlite) to the new
   trait surface
3. Add `Cla::write(&dyn Receiver<Bytes>, total_len)` returning
   `ForwardBundleResult`; non-streaming adapter wraps `forward()`
4. Implement egress executor: spawn `load()`, drive Transformer chain,
   feed `Cla::write()`'s channel
5. Implement BPSec integrity filter — uses Editor + HMAC primitives
   to produce wrapping Transformer (§7.1.4)
6. Implement BPSec confidentiality filter — uses Editor + AES-GCM
   primitives (header-block targets only in this phase)
7. Remove `Signer` and `Encryptor` structs, `BlockSet` trait,
   `EditorBlockSet`
8. Retain `flatten()` / `flatten_inplace()` for tests and small
   bundles

### Phase 3: Streamed Ingress

1. Build streamed parser wrapper (accumulate + retry on
   `NeedMoreData`)
2. Add `Sink::write(&dyn Receiver<Bytes>, ...)` returning
   `Result<()>`
3. Implement pre-filter gate between header parse and payload
   streaming (EarlyIngress / EarlyOriginate hooks)
4. Implement ingress Verifier for CRC and BIB validation
5. Implement ingress Transformer for payload decryption
6. Update TCPCLv4 to push transfer segments directly into a
   `Receiver<Bytes>` and call `Sink::write()`

### Phase 4: Filter API Migration

1. Migrate `ReadFilter` / `WriteFilter` to use `&dyn Receiver<Bytes>`
   for input and `&dyn Sender<FilterOut>` for write-filter output
2. Remove `(Bundle, Bytes)` filter signatures
3. Implement parallel read filter fan-out via `Bytes::clone()`
4. Implement write filter chaining via stream traits

### Phase 5: Security Gateway

1. Implement streaming AES-GCM wrapper for payload BCB
2. Implement inline decrypt Transformer for ingress
3. Deferred commit model for unverified payloads
4. Pre-filter BPSec policy integration

### Phase 6: Integration with Queue Architecture

1. Wire Ingest block into queue model (CLA `Receiver` input,
   `Sender` output to dispatch queue with priority from `flow_label`)
2. Wire ClaSend block into queue model (`Receiver` input from
   per-peer CLA queue)

## 12. Type Safety and Bundle Ownership

RAII is used for `GenerationGuard` (§4.3) — wraps the in-flight
`store()` task and its cancellation token. Drop without commit
cancels the token, aborting the spool. This is the one resource that
benefits from RAII: an uncommitted spool write must be cancelled to
avoid leaked temp files.

For bundle data and metadata themselves, RAII does not apply: both
live in storage backends, the pipeline passes keys (not handles), and
`Drop` is synchronous while storage operations are async. An orphaned
bundle is bounded by its lifetime field; the reaper expires it,
recovery reconciles it.

Typestate within processing blocks (ensuring a bundle passes through
required gates before enqueue) is valid in the durable queue model
but deferred — each processing block is compact and the machinery
cost exceeds the safety benefit at the current codebase size.

## 13. What This Does Not Change

- **MetadataStorage** — bundle identity, status, queue assignment,
  polling, recovery all remain as-is. The existing `Sender<Bundle>`
  surface for poll methods is unchanged.
- **BPSec header crypto primitives** — unchanged; BIB/BCB on
  extension blocks are now driven by the Transformer chain.
- **Dispatch, EgressController, Deliver, Admin, Reassemble** — these
  processing blocks work on `BundleMetadata`, not raw bytes.
- **Recovery protocol** — three-phase recovery continues; in-flight
  spool task temp files (no metadata reference) are cleaned up on
  startup.
- **Reaper** — operates on expiry indexes in `MetadataStorage`,
  deleting from `BundleStorage` as ground truth.
- **Bundle data cache** — remains an LRU cache of small bundles;
  earlier proposals for header segment caching are dropped.
