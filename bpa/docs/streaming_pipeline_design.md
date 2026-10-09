# Design: Streaming Bundle Pipeline

| Document Info | Details |
| --- | --- |
| **Component** | BPA — Bundle data I/O, spool-based streaming |
| **Scope** | Streaming ingress and egress, spool commit model, sequential-only storage |
| **Status** | Living doc — partially implemented (streaming parser, CLA segment delivery, unified ingress pipeline with pre-drain gate + `ValidatingReceiver` validating drain landed; storage / egress pending) |
| **Related** | `queue_architecture.md`, `storage_subsystem_design.md`, Editor (`bpv7/src/editor.rs`) |

## Implementation status (2026-09-03)

This is a living document: the sections below mix shipped behaviour with design targets.

**Landed**

- Streaming parser — `bpv7::parse::BundleParser` (`push` / `finish`), `ParserProgress::{NeedMore, Ready, Partial}`, one-shot sugar `bpv7::parse::parse(Bytes) -> Parsed`. `Partial { consumed, tail: PayloadTail }` is the deferred-payload signal: `push` returns it at the payload boundary and the caller drains the remaining payload through `PayloadTail` (CRC + termination + anti-smuggling checks) without re-buffering headers. Non-canonical CBOR is a hard parse error (§5.2.2). Keyless BPSec structural checks are `bpsec::{bib,bcb}::OperationSet::check`.
- CLA segment delivery — `cla::Segment::{Next, Final}` and the streamed-only `Sink::dispatch(&mut dyn Receiver<Segment>)` (the buffered/streamed trait pairs were collapsed; no `Bytes` variant remains). Stream traits `Sender<T>` / `Receiver<T>` live in `bpa::stream` with adapters over `hardy_async::channel`.
- Streamed ingress drain — `dispatcher::ingress` drives the parser per `Segment` and drains the bundle through a `ValidatingReceiver` (below, yielding the resident head first so one receiver carries the whole bundle) into one `Store::save_stream(&mut dyn Receiver<Segment>)` call: the store owns the spool, so the pipeline already assumes an asynchronous streaming store, and the reject paths already stage-then-discard as the streaming contract requires.
- Pre-drain gate — `bundle::parse::parse_headers` + `HeaderVerify::gate_reason`: lifetime/hop/expiry rejection, the config-gated RFC 9171 validity checks, and the Ingress filter chain all run on the accumulation buffer before the payload drain (§5.4's early reject in interim form; link-layer reach-back per §1.3 rides the CLA's streaming dispatch path). A gate drop is pre-custody and never persisted. The RIB lookup also runs at the gate as the routing decision of record — an explicit Drop route rejects here before the drain begins, with nothing spooled, and the commit executes the carried decision (§5.7's routing-at-commit, both halves).
- `ValidatingReceiver` validating drain (§5.5's pull-through) — the dispatcher's shared `Receiver<Segment>` decorator, wrapped by an input door over its arrival stream: as each segment flows through it feeds the sync `PayloadTail` checks (payload CRC, block/outer breaks, anti-smuggling) plus one incremental digest per deferred payload BIB, then yields the same segment onward; the categorised verdict (`Truncated` / `Invalid` / `IntegrityFailed`) is read from `finish()` after the drain returns, and the door owns the discard of a save the verdict rejects. Layering is as designed: `PayloadTail` and the digest framing are `bpv7`'s (no_std, sans-IO), the async wrapper is bpa's — and validation is the door's, so the store drain (`Store::save_stream`) stays validation-blind and any door can drive it (the originate doors decorate their own streams).
- Deferred payload-BIB verification is streaming — `checks::begin_payload_verification` begins a `bpsec::bib::Verifier` per BIB the header verify deferred, *inside `parse_headers`' keyed scope* (`HeaderVerify::deferred_verifiers`): the `!Send` `KeySource` is resolved once per bundle and never crosses an `await`; only the `Send` verifiers — copied key material, the recorded exception documented on `bib::Verifier` — ride the drain. `checks::verify_payload` and `finalize_with_provider` are deleted, resolving the key-material re-adjudication this block previously flagged.
- Unified ingress pipeline, one metadata write — the CLA, reassembly, and restart in-feeds share `process_received_bundle`; the single `insert_metadata` at `Dispatching` carries the Ingress chain's classifier deltas (`New` is an in-memory marker that never persists), and the gate's routing decision then executes directly — a fresh arrival does not transit the dispatch queue (`DispatchPending` belongs to the re-dispatch paths).
- No editing on input — the bundle is stored exactly as received; RFC 9172 §5.1.1 failure-drops and unrecognised-deletable removals are *scheduled* in `BundleMetadata::to_remove` and applied per attempt at the output doors, ahead of the Egress Rewriters and ahead of the Deliver chain. Captured payload-BCB op-sets are likewise never spent at ingress — delivery re-derives them from the stored bytes (Phase D's streamed payload decrypt makes that streaming).
- Parse-mode collapse (§9.3) — the three `parse_{preserve,canonicalize,full}_with_provider` pipelines are gone; call sites compose the primitives (`bpa::bundle::parse`).
- Decoded extension fields — pre-parsed at ingress into `BundleMetadata` (§2.3's open choice is resolved; the filter redesign partitions them as the **wire cache** group). The rich `bpa::Bpv7Bundle` view is replaced by structural `hardy_bpv7::Bundle` + metadata.
- `RewrittenBundle` and the `Checked` / `Rewritten` / `Parsed` taxonomy removed from `bpv7`.
- Streamed egress without cryptographic stages (§6.3) — `forward_bundle` opens the stored bundle with `Store::load_stream`, the streaming read seam, and holds it only until the bytes held reach the payload's data (`dispatcher::output::pull_headers`): every header the output stages edit or read is then resident. One editor over those bytes serves the attempt — the scheduled removals, the Egress Rewriters and the per-hop writes — and rebuilds once, and the rebuild's chunks reach `Cla::forward` as segments (`dispatcher::output::ChunkReceiver`): the outer array's head, an edited block's new bytes, a kept block's resident bytes zero-copy, and the stored payload, the rest of the load stream after the resident bytes. `total_len` is the rebuilt block index's `encoded_len()`. Nothing on the egress path parses or flattens the bundle.

**Interim (works, not yet the target shape)**

- The spool's destination is still RAM: `Store::save_stream` accumulates its segment stream (bounded by `max_bundle_size`) and commits via `BundleStorage::save(Bytes)` on the final segment. The seam now lives *inside the store wrapper*, behind the target signature — one `Receiver<Segment>` in, storage name out — so the storage-spool tranche swaps `save_stream`'s body (and the backend trait) for the streamed write (§3) without touching pipeline code: every stage upstream already sees a plain `Receiver<Segment>`, and ingress already deletes a staged save whose post-stream validation fails (the discard half of the contract).
- The load's source is RAM too: `Store::load_stream` loads the whole buffer through `BundleStorage::load` and yields it as one `Final`, so the resident bytes are the whole stored bundle and an Egress Rewriter reads the payload. The output door already pulls only through the header region, so the storage-spool tranche swaps `load_stream`'s body without touching pipeline code.
- The drain follows the gate decisions: the door drains its decorated stream through one `Store::save_stream` call only once the chain and the route lookup have settled, and a rejected arrival spools nothing (§5.7's ordering, landed); a Classifier-set storage QoS that the spool must honour is a policy-tranche target. What remains interim is only the spool's RAM destination above.

**Pending**

- Streaming storage (§3 — `BundleStorage` still exposes `save(Bytes)` / `load -> Result<Option<Bytes>>`; swapping `Store::save_stream`'s RAM accumulator for the backend's streamed `store()`, and `Store::load_stream`'s whole-buffer body for the backend's streamed load, are the remaining prerequisites for the payload-never-in-RAM property §2.5), the egress cryptographic stages (§6.1.1), delivery's streamed output, the §10 phases.

Where a section below still describes the pre-implementation shape, the API names in this block are authoritative.

## 1. Background

Hardy 0.1.0 was released as a working in-memory BPA: bundles arrive on the CLA, are reassembled into a contiguous `Bytes`, parsed, stored, optionally filtered and rewritten, then forwarded. The pipeline is correct, but the data path is single-shot — every stage receives the bundle as a complete `Bytes`, and the parser, filters, storage backends, and CLAs all materialise the whole payload in RAM before doing their work. For small bundles in laboratory traffic this is fine.

After the 0.1.0 release, the Aqueduct project surfaced three pressures that the single-shot model cannot meet under realistic deployment conditions. These are the problems the streaming pipeline is here to solve.

### 1.1. Internal Prioritisation and Flow Control

With whole-bundle ingress and egress, a single large bundle in transit holds the pipeline against anything behind it — the BPA cannot make a scheduling decision until the bundle is fully received, parsed, and queued. Once bundles flow as streams of chunks, the BPA can interleave them, schedule chunks from higher-priority bundles ahead of lower-priority ones, and apply per-source / per-destination flow control by managing channel depths. A 1GB low-priority bundle stops being a wall that the urgent bundle behind it has to wait behind.

For Hardy's intended deployments (mixed-priority space DTN links, multi-tenant gateways) this is the dominant pressure, even though it is the least visible. It is also the reason channels appear at the per-call, per-bundle grain rather than at the per-CLA grain — see §5.1.

### 1.2. The 1GB Bundle as a Memory Ceiling

The single-shot pipeline materialised the whole bundle in RAM at every stage. For payloads of many MB this is not acceptable, and in space DTN it is not even close to acceptable. §2.5 catalogues the materialisation points and which remain (storage write/read and egress); peak per-bundle resident memory drops to the size of the header blocks (kilobytes) while the payload spools sequentially from CLA to disk on ingress and from disk to CLA on egress.

### 1.3. Early Reject Reaches the Link Layer

Rejecting after the header parse and before the payload arrives does not just save the BPA work — it propagates back to the wire. A TCPCLv4 CLA can issue XFER_REFUSE and reclaim the TCP transfer slot; a UDPCLv2 CLA stops accepting datagrams for the bundle in question; on a constrained radio link, an aware CLA can back off or tear the link down.

On a space DTN where bandwidth and energy are both scarce, this is not a micro-optimisation — it is the difference between burning a contact window on rejected traffic and not. This is what makes the pre-drain gate (§5.4) load-bearing rather than convenient.

## 2. Architecture

### 2.1. The BPA Does Not Pass Bundles Around

The BPA is a stateless pipeline over durable state. Bundle data and metadata both live in storage backends. The pipeline passes keys and lightweight metadata views — indices into durable state, not the state itself. Processing blocks dequeue a key, read what they need from storage, make a decision, and enqueue the key to the next queue.

The total data under management may far exceed available RAM. Design proposals should be evaluated against this model: if a concept requires in-memory ownership of bundle data, it does not fit.

### 2.2. The Metadata/Data Split Is Preserved

`MetadataStorage` owns bundle identity, status, and queue assignment. `BundleStorage` owns raw bytes. These remain separate traits with independent implementations.

### 2.3. Bundle as PrimaryBlock plus Block Index

The `Bundle` struct is **concrete** — a decoded `PrimaryBlock` alongside a `HashMap<u64, Block>` recording the structural index of extension blocks (byte extents, block types, flags, CRC types). The `PrimaryBlock` fields (source EID, destination EID, creation timestamp, lifetime, report-to EID, flags, CRC type) are decoded by the parser and live in `Bundle::primary` — they are the BPA's authoritative view of the bundle's identity and routing information, and the extra cost of carrying them parsed (rather than re-decoding from the wire bytes on demand) is warranted given how often the BPA needs them. Extension-block content is **not** decoded into the `Bundle`. Where the BPA pipeline needs decoded extension-block values, they are pre-parsed at ingress into `BundleMetadata` — the choice is resolved: `PreviousNode`, `BundleAge`, and `HopCount` are cached there (the filter redesign's **wire cache** group — a parser-derived cache of the stored bytes, never invalidated because stored bytes are immutable); anything else is decoded on demand from the block's byte extent.

`BundleMetadata` is the BPA-internal pipeline-state structure; `filter_subsystem_design.md` partitions it into provenance / wire cache / classification / infrastructure groups with per-group visibility. The design intent is unchanged: it does not duplicate primary-block fields, which the BPA reads from `Bundle::primary`.

Extension-block edits (the per-hop blocks, a Rewriter's edits) are planned against the block index and applied to the resident header region; the payload streams past untouched. The only stages that transform the byte stream are the hard-coded cryptographic ones (§6.1). Full struct details and crypto readiness in §7.

### 2.4. Sequential Write, Sequential Read

Bundle data I/O is fully sequential in both directions:

- **Write (ingress)**: bytes arrive from the CLA in order and are written to storage sequentially. This is a **spool** — data flows through, is committed atomically, and is never modified in place.
- **Read (egress)**: stored bytes are read sequentially. The edited header region is emitted first, then the payload streams through untouched — passing the hard-coded cryptographic stages where this node adds BPSec, which capture header data as it flows past (CRC computation, BPSec IPPT/AAD construction). No random access is required.

Sequential writes are the best-case I/O pattern for every backend (disk, SSD, NOR flash, S3). The sequential write model also enables **early drop**: the BPA can parse headers from the accumulation buffer and run the pre-drain gate before the payload arrives. If rejected, the CLA cancels the transfer mid-stream — the payload is never received or stored (§5.4).

### 2.5. Memory Impact

The 1GB memory ceiling (§1.2) is the most concrete demonstration of the architecture's effect. The tables below catalogue where Hardy's existing pipeline materialises bundle data in RAM, and what changes once the streaming architecture is in place.

#### 2.5.1. Bottlenecks

| # | Stage | Bottleneck | Memory | State |
|---|-------|-----------|--------|-------|
| 1 | CLA reception | reassemble entire bundle before dispatch | 1GB alloc | resolved — segment streaming (§5.1) |
| 2 | Parse | whole-buffer parse needs complete `&[u8]` | 1GB resident | resolved — streamed parser (§5.2) |
| 3 | Storage write | `BundleStorage::save(Bytes)` — bundle in RAM through the write | 1GB I/O | remains — §3 / Phase A |
| 4 | Reject timing | cannot reject until fully received and stored | wasted | resolved — pre-drain gate (§5.4) |
| 5 | Storage read | `BundleStorage::load()` reads the whole bundle back | 1GB alloc | remains in the backends — §3 / Phase B; the egress door already pulls only its header region through `Store::load_stream` |
| 6 | Editor flatten | `flatten`/`flatten_inplace()` may need a second buffer | 1-2GB | resolved at egress — the rebuild's chunks travel as segments (§6.3); delivery still flattens once |
| 7 | CLA forward | `Cla::forward` receives the loaded bundle as one whole-buffer segment, and buffering CLAs hold it while transmitting | 1GB resident | partly resolved — the bundle arrives as segments with a plan-derived `total_len` (§6.2); buffering CLAs still hold it (`INTERIM BUFFERING`, Phase B) |

#### 2.5.2. With This Design

| # | Stage | Change | Memory |
|---|-------|--------|--------|
| 1 | CLA reception | Chunks arrive via channel, no reassembly | Bounded |
| 2 | Parse headers | Streamed parser, headers only | Kilobytes |
| 3 | Pre-drain gate | Runs after headers, before payload | — |
| 4 | Payload to storage | Spools from CLA through pipeline to disk | Bounded |
| 5 | Egress | Stored bytes stream to the CLA, through the cryptographic stages where BPSec is added | Bounded |
| 6 | CLA forward | Chunks via channel, no contiguous buffer | Bounded |

**Peak memory for 1GB bundle: kilobytes** (header blocks in the parser's accumulation buffer or cache). The payload spools sequentially from CLA to disk at ingress and from disk to CLA at egress.

## 3. BundleStorage Trait

### 3.1. Trait Surface

The trait defines three primitives — store, load, delete — using the stream traits from §4:

```rust
#[async_trait]
trait BundleStorage {
    /// Pull bundle bytes from `stream` into a new spool. The receiver
    /// yields `Ok(Segment::Next(bytes))` for non-final payload
    /// segments and `Ok(Segment::Final(bytes))` for the final segment
    /// (which may be zero-length); on `Final`, commit the spool and
    /// return the assigned storage name. `Err(Disconnected)` (the
    /// producer dropped without sending `Final`) is treated as abort
    /// and the staged data is discarded. See §3.2 for the full
    /// Segment contract.
    async fn store(
        &self,
        stream: &dyn Receiver<Segment>,
    ) -> Result<Arc<str>>;

    /// Push the stored bundle bytes for `storage_name` into `stream`
    /// in order, returning when all bytes have been pushed or the
    /// consumer has gone away (`SendError`). The egress direction is
    /// a pure stream — end-of-data is unambiguous, so no `Segment` is
    /// needed.
    async fn load(
        &self,
        storage_name: &str,
        stream: &dyn Sender<Bytes>,
    ) -> Result<()>;

    /// Delete stored bundle data.
    async fn delete(&self, storage_name: &str) -> Result<()>;
}
```

Both `store` and `load` are fully sequential: every backend (local disk, SSD, NOR flash, S3, tape) handles these primitives naturally. The stream traits (§4) decouple the byte channel from the trait surface — each backend wraps whatever transport is most convenient.

A **bounded head read needs no extra primitive**: the consumer calls `load` and drops its receiver once it has the bytes it wants (headers plus any declared payload peek); the backend sees `SendError` and stops streaming. Restart re-admission uses exactly this shape to re-supply filter invocation data (`filter_subsystem_design.md`), so the sequential-only model holds there too.

The `store` / `load` names match Hardy's existing storage convention and are unambiguous because the BPA is always the caller. The CLA byte-streaming surfaces keep their established verbs rather than the §4 `write` / `read` convention: egress is the streamed `Cla::forward` (§6.2), ingress the streamed `Sink::dispatch` (§5.1.3).

### 3.2. Commit and Abort

The spool needs to distinguish "producer finished, commit this" from "producer abandoned the write, throw it away". The distinction is carried in-band at the trait surface by a small enum threaded through the receiver's item type:

```rust
pub enum Segment {
    Next(Bytes),
    Final(Bytes),  // the final segment; may be zero-length
}
```

A trait method that takes `&dyn Receiver<Segment>` is declaring that its producer marks the final item with `Final(bytes)` (which may carry payload data or be empty), and that a bare disconnect (the producer dropped without ever sending `Final`) is to be treated as abort. Plain `Receiver<Bytes>` is reserved for streams where end-of-data is unambiguous (e.g. `BundleStorage::load`, where the BPA is itself the producer and a consumer disconnect just means "stop streaming").

The consumer pattern is uniform — *append until `Final`, then commit*:

```rust
loop {
    match stream.recv().await {
        Ok(Segment::Next(bytes)) => /* append, keep going */,
        Ok(Segment::Final(bytes)) => { /* append (may be empty), commit */; break },
        Err(Disconnected)      => break /* abort: producer dropped */,
    }
}
```

There is no explicit `Abort` variant — `Err(Disconnected)` is the abort signal. The producer aborts by simply dropping (or closing) its sender without first sending `Segment::Final`. Folding the terminator into a data-carrying variant means the producer doesn't have to make a separate "I'm done" round-trip for the trailing bytes; the common case (last network segment, last edited block) sends one fewer item, and the degenerate case (no trailing data) is just `Final(Bytes::new())`.

The trait surface stays implementation-agnostic. Internally, the producer side typically holds a `closeable::Sender<Segment>` and follows the convention "send `Segment::Final(_)` then drop to commit; drop without `Final` to abort" — but a trait implementer is free to choose any transport that yields the right sequence at the receiver. No wrapping guard type is needed: the `Sender` itself enforces the commit/abort distinction by virtue of the protocol.

Abort's operational use is the failed drain: a stream that ends without `Final` — the producer cancelled, or a pull failed the drain's validation — commits nothing (a payload-BIB failure, which can only settle once the whole payload has streamed past, discards the staged save instead, §5.7).

## 4. Stream Traits: Sender and Receiver

> **Landed state (2026-08).** The pull-side foundations of this section are now real, with the landed shape diverging from the sketch below in naming and location, not substance: `Receiver<T>`, `RecvError` (a unit struct), `Segment`, and the interim `concat_stream` (size-capped, truncation-as-error) live in `bpa::stream` rather than `hardy-async`, with blanket impls on the `hardy_async` channel endpoints in place of adapter types. The trait seams are `cla::Sink::dispatch` and `services::ServiceSink::send` (streamed-only since the buffered/streamed pairs were collapsed; a caller holding a whole buffer passes `&mut Bytes` directly — `Bytes` implements `Receiver<Segment>`, draining as a single `Final` segment), not the `write()` naming sketched in this section. A producer death before `Segment::Final` is a truncation error surfaced to the door (`StreamCancelled` — a CLA withholds its transfer ack), reassembly is bounded by `BpaBuilder::max_bundle_size` (`max-bundle-size` in bpa-server config), and registration liveness is enforced per segment by sink-side receiver wrappers. The forward design below is otherwise unchanged and still governs the remaining work.

The pipeline streams items between components without coupling the trait surface to a specific channel implementation. The storage subsystem already establishes the **`Sender<T>` pattern** for this — see [storage_subsystem_design.md](storage_subsystem_design.md) §"Streaming results via `Sender<T>`" for the canonical definition and rationale. This section reuses that pattern across the BPA's storage, CLA, and filter trait surfaces.

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

The pull-side dual is `Receiver<T>` — reserved name in the storage docs, defined here for the byte-streaming pipeline:

```rust
/// Pull-side stream: consumer drives.
#[async_trait]
pub trait Receiver<T>: Send {
    /// Returns `Ok(item)` for each value, or
    /// `Err(RecvError::Disconnected)` once the producer has finished
    /// (last `Sender` dropped or `close()` called) and the buffer is
    /// drained.
    async fn recv(&mut self) -> Result<T, RecvError>;
}

pub enum RecvError {
    Disconnected,
}
```

The `Result`-based shape (not `Option`) mirrors `closeable::Receiver` in `hardy-async` directly. Trait surfaces that need to distinguish graceful end from abort do so by carrying a `Segment` item type (§3.2); for those, `Err(Disconnected)` means "producer dropped without sending `Segment::Final`" — i.e. abort.

The two traits are deliberately asymmetric in their receivers: `send` takes `&self` (fills commute — concurrent producers may share a sink), while `recv` takes `&mut self` (a receiver is a drain — each pull is a state transition of the one consumer's totally-ordered view, so exclusive access is the semantic contract, and implementors need no interior mutability). They are passed as `&dyn Sender<T>` / `&mut dyn Receiver<T>`, preserving object safety on the traits that consume them (`BundleStorage`, `MetadataStorage`, `Cla` — all held as `Arc<dyn ...>` chosen at runtime). The cost of `dyn` is one indirect call per item, negligible relative to underlying transport work.

Channel adapters live separately from the traits. The storage subsystem provides `ChannelSender<T>` wrapping `hardy_async::channel::Sender<T>`; the mirror `ChannelReceiver<T>` wraps `hardy_async::channel::Receiver<T>`. Tests use `Mutex<Vec<T>>`-backed collectors. None of these adapters appear in the trait surface.

Byte streams use `Sender<Bytes>` and `Receiver<Bytes>` directly — no specialised aliases.

**`Closeable` as the implementation primitive.** The `Sender<T>` / `Receiver<T>` pair is the natural abstraction over `Closeable` in `hardy-async`, which combines a channel with a cancellation token behind a single interface. Implementations that want cancellable cleanup — the registry's reconciler task (§5.1.2), per-call streaming receivers passed into `Sink::dispatch_streamed`, anywhere a `select_biased!(cancel | recv)` pattern would otherwise be hand-rolled — compose naturally on top of `Closeable` without re-exposing the cancellation primitive in their trait surface.

**Direction conventions for trait method parameters:**

- A method that takes `&dyn Sender<T>` *pushes items into it*; the caller holds the receiving end. Example: `BundleStorage::load(_, stream: &dyn Sender<Bytes>)` — storage pushes bytes; the caller wraps a channel receiver and reads from it.
- A method that takes `&dyn Receiver<T>` *pulls items from it*; the caller holds the producing end. Example: `BundleStorage::store(stream: &dyn Receiver<Segment>)` — storage pulls bytes; the caller wraps a channel sender and writes into it.

**Method naming on symmetric trait surfaces.** When a trait surface is implemented on either side of a connection (CLA traits, filter traits), `stream_in()` / `stream_out()` are ambiguous — "in" relative to whom? These traits use `write()` for the push direction and `read()` for the pull direction, naming what the *caller of the method* is doing. A CLA that calls `sink.write(stream)` is writing bytes; the BPA that calls `cla.write(stream)` is also writing bytes. The trait surface is identical; only the direction of data flow differs by use site.

Storage avoids the ambiguity entirely because the BPA is always the caller — `store()` / `load()` therefore stay with their existing Hardy names.

## 5. Ingress: CLA to Storage

### 5.1. Sink Pattern and CLA Segment Delivery

#### 5.1.1. Sink as Factory for Per-Bundle Channels

The Sink stays as a `dyn Trait` — the abstraction boundary that lets the proto crate substitute a gRPC-streaming implementation for the local registry implementation invisibly to the CLA. What changes is the shape of the data-plane methods: rather than receiving complete bundles in memory, the Sink **manufactures a fresh per-bundle channel inside each `dispatch_streamed()` call**, which lives only for the duration of that bundle. The Sink itself is not a long-lived channel; it is a factory that produces ephemeral ones on demand.

This framing matters for a handful of decisions that fall out of it:

- **Control-plane methods stay as `async fn -> Result<T>`.** `add_peer`, `remove_peer`, and other helper operations on the Sink are infrequent, need typed replies, and have no streaming or backpressure requirement. Channelising them adds tax without benefit.
- **Per-call channel granularity preserves priority and per-stream fan-out.** A single shared channel per CLA would head-of-line block on multi-priority egress (`Cla::forward(lane, ...)` — see §6.2) because ordering is cemented before the CLA implementation sees the bundle, and on multi-peer ingress because concurrent sessions serialise through one consumer. Per-call channels avoid both: priority is honoured because the BPA selects which call goes to the CLA next, and multi-peer ingress fans out naturally because each session manufactures its own channel.
- **Each call carries its own cancellation token** (passed to `write()` or `forward()`), scoped to one bundle's worth of work. There is no cancellation hierarchy mirroring the Sink's lifetime; the Sink's own lifecycle is handled separately (§5.1.2).

#### 5.1.2. Sink Lifecycle and Ownership

The Sink trait and its ownership stay. The registry holds the registration (`Arc<Cla>`, `Arc<Service>`; the RIB tracks routing agents by name), the component owns the `Box<dyn Sink>` it receives in `on_register`, and the Sink refers back to the registration — a `Weak` that each call upgrades, or the RIB's agent lookup — so a call after unregistration fails `Disconnected`. Dropping the Sink unregisters the component, so a component keeps its Sink for as long as it stays registered. The piece that needs rework is how that drop is delivered: each Sink's `Drop` impl spawns the async cleanup, the well-known spawn-from-Drop anti-pattern.

The target shape:

- **Liveness signal via sync drop, async reconciler.** Each concrete Sink holds a small drop-detector struct whose only job is to push its `ComponentId` onto an `mpsc::UnboundedSender<ComponentId>` when the Sink is dropped. Unbounded because `send()` must never block — it is called from `Drop`. The receiver lives in the registry, drained by a long-lived reconciler task that calls the internal unregistration path. This removes the spawn-from-drop anti-pattern (Drop is plain sync code that pushes one ID and returns), fires exactly once when the component drops its Sink, and composes cleanly with the explicit happy path: if `unregister()` was already called, the reconciler sees a stale ID and no-ops.

- **Explicit `unregister()` stays the documented happy path.** `sink.unregister().await` runs the unregistration path directly; subsequent calls on the Sink fail `Disconnected`, and the component's later drop of the Sink finds nothing to do.

- **BPA-initiated shutdown.** `registry.shutdown()` unregisters every remaining entry (calling each `component.on_unregister()`), then drains the reconciler before the registry's task pool shuts down. Components see `Disconnected` on subsequent calls.

- **Proto crate impact is minimal.** The remote Sink impl in proto already holds its own state and lifecycle; the same drop-detector pattern lives inside it, with the reconciler closing the gRPC stream when the local-side Sink is dropped. The split reader/writer `RpcProxy` is largely unchanged.

The reconciler loop uses `Closeable` (§4) rather than a hand-rolled `select_biased!(cancel | recv)` shape. The same lifecycle design applies uniformly to `ServiceSink`, `ApplicationSink`, and `RoutingSink` — one pattern across all registries.

Inverting the ownership — the component holding `Arc<dyn Sink>` and the registry a `Weak<dyn Sink>`, with one `alive: AtomicBool` replacing the per-call `Weak::upgrade()` — was considered and rejected: the component owns its Sink.

#### 5.1.3. CLA Segment Delivery API

Landed differently than sketched below: the buffered `Sink::dispatch(Bytes)` was **not** retained — the buffered/streamed trait pairs collapsed into a single streamed-only `Sink::dispatch(&mut dyn Receiver<Segment>)` (see the Implementation status block). The `Segment` pattern from §3.2 carries the same commit-versus-abort requirement as storage write (end-of-bundle is `Segment::Final`; a mid-transfer abort such as TCPCLv4 XFER_REFUSE is a bare disconnect). Original sketch:

```rust
trait Sink {
    // Existing — full bundle in memory
    async fn dispatch(&self, data: Bytes, ...) -> Result<()>;

    // New — streaming dispatch. CLA passes a Receiver<Segment>;
    // BPA pulls `Segment::Next(bytes)` chunks until `Segment::Final`
    // (commit, dispatch the bundle) or `Err(Disconnected)` (abort,
    // discard any staged ingress state).
    async fn dispatch_streamed(
        &self,
        stream: &dyn Receiver<Segment>,
        ...
    ) -> Result<()>;
}
```

Streaming CLAs (e.g., TCPCLv4) construct a bounded channel, spawn a task that pushes each transfer segment as `Segment::Next` into the sender, and call `sink.dispatch_streamed(&stream, ...)` to drive ingest. Backpressure propagates naturally through the bounded channel to TCP flow control. End of bundle is signalled by the CLA sending `Segment::Final`; the BPA observes it and finalises ingress. If the transfer is aborted (e.g., TCPCLv4 XFER_REFUSE arrives mid-stream), the CLA drops the sender without sending `Final` — the BPA sees `Err(Disconnected)` and discards the staged ingress state.

If the BPA stops reading mid-stream — a gate rejection, or an early acceptance that needs no further bytes (a duplicate identified from the headers) — `dispatch()` returns early and the CLA's pushing task sees `SendError` on its next push. That is only "stop pushing": the verdict rides `dispatch()`'s returned `Acceptance` — `Accepted` = acknowledge the transfer (on TCPCLv4 a still-open wire transfer is refused as already-complete rather than acknowledged); `Refused` = withhold the acknowledgement and tear down the wire transfer (e.g., XFER_REFUSE). An `Err` is never a verdict — it means the sink itself has failed.

The BPA core only implements the streaming ingress path. (Implemented as the streamed-only `Sink::dispatch`; the §4 `write`/`read` naming convention was not adopted for this ingress method — it kept the `dispatch` family. The buffered `dispatch(Bytes, ..)` convenience existed as a provided default while the pair coexisted, and was removed when the pairs collapsed — callers pass whole buffers directly, `Bytes` being a `Receiver<Segment>`. The CLA egress method likewise kept `forward` (§6.2), and filters take no byte streams (§5.3).)

**Streamed-only surfaces.** Neither `Sink::dispatch` nor `Cla::forward` (§6.2) has a buffered variant: every CLA reads from a network stream, so a whole-bundle method would only make it materialise the bundle artificially. A CLA that genuinely needs a contiguous bundle buffers explicitly with `stream::buffer_stream`.

### 5.2. Streamed Parser

The parser lives in `bpv7::parse` as `BundleParser` and exposes a two-phase API:

```rust
pub enum ParserProgress {
    NeedMore(usize),
    Ready(Bytes),
    /// Headers and all BPSec blocks parsed, but the payload body exceeds
    /// the buffer. `consumed` is everything received so far; `tail` is a
    /// synchronous continuation (payload CRC, block/outer termination,
    /// anti-smuggling checks) the caller feeds while draining the rest.
    Partial { consumed: Bytes, tail: PayloadTail },
}

impl BundleParser {
    pub fn new(chunk_size: usize) -> Self;

    /// Phase 1: push each incoming chunk. Returns `NeedMore(n)` while
    /// the header region is short, `Ready(bytes)` once the bundle is
    /// complete and its payload trailer verified (`bytes` = the full
    /// concatenation, ready to relay into the spool in one piece), or
    /// `Partial` once the payload block's header has parsed and the
    /// rest, with its verdict, is the tail's (drain via `PayloadTail`).
    pub fn push(&mut self, data: Bytes) -> Result<ParserProgress, Error>;

    /// Phase 2: run the BPSec cross-block structural validation against
    /// the final byte buffer, returning the parsed `Bundle` index, the
    /// authoritative `Bytes`, and the pre-parsed BIB and BCB
    /// OperationSets, bundled as `Parsed`.
    pub fn finish(self, data: Bytes) -> Result<Parsed, Error>;
}

/// Output of `finish` / `parse`: the authoritative byte buffer alongside
/// the structural bundle and decoded BPSec OperationSets.
pub struct Parsed {
    pub data: Bytes,
    pub bundle: Bundle,
    pub bcbs: HashMap<u64, bcb::OperationSet>,
    pub bibs: HashMap<u64, bib::OperationSet>,
}

/// One-shot sugar over the streaming primitives (tools, tests, round-trips).
pub fn parse(data: Bytes) -> Result<Parsed, Error>;
```

Internally a small state machine (`Start → PrimaryBlock → Blocks → Done`) drives the walk. Inner CBOR parsing functions operate on `&[u8]` slices of the accumulation buffer. Header blocks are retained in the buffer until `finish()` returns; the buffer is the random-access substrate for CRC validation and early-filter inspection without storage I/O.

Splitting the structural walk (`push`) from BPSec validation (`finish`) lets the pre-drain gate validate the resident headers — BPSec structure and, with keys, header BIBs — before any payload is drained, so an invalid or policy-rejected bundle is rejected having spooled nothing, and the keyed pass runs inside one synchronous scope.

#### 5.2.1. BPSec Verification at the Gate

**Keyed** BPSec operations (BIB hash verification, BCB decryption / tag verification) are **not** performed by the parser. They run at the pre-drain gate as fixed, KeyProvider-driven machinery, keeping key material and security policy out of the parser.

**Keyless** structural validation of BPSec blocks **is** performed by the parser. RFC 9172 defines a set of inter-block structural rules — BCB MUST NOT target the primary block (§3.7), BCB MUST NOT target another BCB (§3.7), BIB MUST NOT target a BCB (§3.9), each block at most one BCB target (§3.9), BCBs MUST NOT set `delete_block_on_failure` (§3.7), BCBs targeting the payload MUST set `must_replicate` (§3.7), security operations unique per `(service, target)` (§2.6), every targeted block number must exist in the bundle — that are pure functions of the parsed `Bundle` index plus the Abstract Security Block contents of each BIB/BCB. No keys, no policy, no I/O.

The parser handles BIBs and BCBs asymmetrically during the walk (zero cost for bundles with no security blocks):

- **BCBs** are decoded inline during the block walk. The ASB (which describes what the BCB encrypts) is itself plaintext, so the parser parses each BCB's `OperationSet` as it sees it.
- **BIBs** are recorded by block number in a pending list and decoded by `finish()`. The deferral is necessary because a BIB may itself be a BCB target — in which case its body is ciphertext until the BCB is decrypted. `finish()` parses every BIB whose body is plaintext; BIBs whose bodies are BCB-protected are skipped (their target blocks get `BibCoverage::Maybe`, deferred to the keyed verification step).

`finish()` returns the `Bundle` index along with the pre-parsed BIB and BCB `OperationSet` maps, so downstream filters don't re-decode. The cross-block rules are applied here; on violation, `finish()` returns `Err` and the bundle is rejected in the header pass, before the gate and with nothing spooled. This is the earliest a structurally-malformed bundle can be rejected. The structural validators are exposed as `bpsec::{bib,bcb}::OperationSet::check` (composed by `bpv7::checks`) so offline tooling can run the same checks without standing up a filter pipeline.

The keyed BPSec verification step (after the parser, with key access) has access to the accumulation buffer (all header blocks in memory) and the `Bundle` block index for structural navigation. Header-block BIBs are verified immediately. Payload-block BIBs require payload data — they are verified incrementally as the payload drains, the `ValidatingReceiver` feeding each chunk to the deferred verifiers (§5.7).

The split — keyless-structural in the parser, keyed-cryptographic in the gate's verification step — is the cleanest mapping of "what changes between deployments" onto layer boundaries. The structural rules are RFC-mandated and identical for every implementation; keys, key sources, and security policy vary by deployment.

#### 5.2.2. Strict Canonicalisation

The streaming parser **rejects** non-canonical CBOR as a hard parse error (`Error::NotCanonical`). There is no flag-and-defer mechanism and no late-stage canonicalisation filter: a bundle that doesn't already conform to RFC 9171 canonical encoding fails parse at the early-filter gate before any spool is opened.

Rejection is chosen over the alternative — flag non-canonical blocks at parse and rewrite them after commit — for three reasons:

- **Spool model fit.** Once bytes are committed, rewriting them in place is exactly the operation the spool model is trying to avoid.
- **Implementation simplicity.** Non-canonical handling required a parallel code path on the hot path; rejection collapses it.
- **Operational reality.** Well-behaved implementations produce canonical CBOR. Non-canonical bundles are an implementation bug at the source, not a workload to accommodate downstream.

There is no `RewrittenBundle` rewrite plan and no `Checked` / `Rewritten` / `Parsed` mode taxonomy in the parse surface (§9.3).

**Parse failures do not generate status reports.** A bundle that failed to parse can't be trusted to identify its source: the primary-block fields the status-report flow would need (source EID, bundle ID, report-to EID) are exactly the fields whose decoding may itself have failed. Status reports are reserved for bundles that parsed successfully but were rejected later — by a filter, by a policy decision, or by an egress failure — where the `PrimaryBlock` is known-valid. There is no best-effort primary-block recovery for corrupted bundles.

A planned future capability will let the BPA **encapsulate** a parse-failed bundle as an opaque payload inside a fresh, BPA- originated bundle addressed to a configured **quarantine EID**, for post-mortem analysis. Until that lands, parse-failed bundles are dropped from the live pipeline and not processed further.

#### 5.2.3. Header Segment Write Constraint

The first chunk pushed into `BundleStorage::store()` MUST contain the **complete header segment** — primary block through all extension blocks — at its start, as a single contiguous run of bytes. This ensures header blocks are stored contiguously at the start of the bundle data.

The first chunk MAY extend past the header segment into the payload. The parser's `push()` returns the full concatenation of bytes it consumed while reaching the payload-header boundary (`ParserProgress::Ready(Bytes)`); whatever the parser had accumulated when it identified the payload block is what flows into the spool's first chunk. For small bundles that arrive in a single CLA chunk, the first (and only) spool chunk is the entire bundle — header + full payload. For large bundles where the payload arrives across many subsequent chunks, the first spool chunk is the headers plus whatever payload prefix happened to sit in the same accumulation buffer.

The contiguity guarantee is "headers contiguous at the start of stored data," not "headers exactly fill the first chunk." This matches what the parser naturally produces and avoids the BPA having to slice the returned bytes at `header_len`.

Payload bytes that arrived after the parser completed continue to stream as subsequent chunks. This means that during egress the header blocks come first, so they can be edited and captured before the payload arrives.

The `Span` model is **not needed at ingress** — it is internal to the Editor. At ingress, the accumulation buffer is never mutated — the bundle is stored exactly as received; the parser is pure and can be tested independently.

### 5.3. Filters in the Streaming Pipeline

Filter design — kinds, hooks, registration, metadata, restart re-admission — is owned by [`filter_subsystem_design.md`](filter_subsystem_design.md). What matters to the byte pipeline is the shape of the contract: all three kinds (parallel **Verifier**; sequential **Classifier** at the input hooks; extension-block **Rewriter** at the output hooks) need no payload to decide (a filter may still read a resident payload), and each boundary has exactly one single-pass hook — Ingress on the pre-drain gate (§5.4), Originate pre-store, Egress in ClaSend before the per-hop writes, which precede the BPSec seam, Deliver before payload decrypt.

The byte contract keeps filters off the streaming path entirely:

- An invocation receives its kind's context: the wire bundle, a reader over the resident prefix — the headers and whatever of the payload has arrived, at least the first min(P, payload length) payload bytes the gate holds for a declared peek (§5.4) — and `payload_peek` for those bytes. A block body not resident reads as the reader's `NotResident`.
- No filter receives a byte stream, holds a stream open, or blocks the drain: the hook runs on the accumulation buffer at the gate, and the payload spools past untouched.
- Classifiers return a `MetadataDelta` that the engine applies — annotation slots, plus the named fields that arrive with their tranches (the `route_table` / `route_key` routing inputs; the traffic `class`, which drives the dispatch enqueue once the policy tranche lands). Filters never mutate stored bytes — the egress Rewriter edits extension blocks per transmission attempt, in memory, so §6.4's read-only forward path holds by construction.
- The originate-raw path (`Dispatcher::originate_raw`, bundles from services via gRPC) runs the same strict parser → gate pipeline as ingress — non-canonical service-provided bytes are rejected at parse (§5.2.2), never canonicalised.

Built-in checks (validity, rfc9171 strictness) are pipeline code gated by `Config`, upstream of any registered filter; BPSec verification and the RFC 9172 §5.1.1 failure-drop are fixed machinery (§5.2.1), never filters.

### 5.4. The Pre-Drain Gate

After all header blocks are parsed and before the payload arrives, the pre-drain gate runs: built-in checks, BPSec header verification, then the Ingress hook (Verifiers in parallel, then Classifiers), then the route lookup — the routing decision of record (§5.7).

If the gate **rejects** (a built-in check, a registered Ingress filter per `filter_subsystem_design.md`, or an explicit Drop route): the BPA returns from `Sink::dispatch()` without commencing a spool. The CLA's pushing task sees `SendError` on its next push and stops; the verdict itself is `dispatch()`'s returned `Acceptance` (the stream closing carries none) — a policy rejection of an invalid-by-headers bundle is *accepted-and-dropped* with reports, while a size-cap trip is `Refused`, from which the CLA cancels the wire transfer (e.g., TCPCLv4 XFER_REFUSE, UDPCLv2 stops accepting datagrams). For a 1GB payload from a rejected source, the BPA has received only the header blocks. Zero wasted I/O.

This is the mechanism behind the **link-layer-reach** motivator (§1.3): the BPA's reject decision propagates back to the wire, the CLA cancels the transfer mid-stream, and the link layer reclaims its resources without ever delivering the payload bytes. It is also effectively **DDoS protection**. Without the gate, a DTN node is trivially DoS-able: an attacker sends oversized bundles with forged sources, and the victim must receive, parse, store, and process the entire payload before deciding to reject. With it, the BPA inspects headers (~hundreds of bytes), rejects, and the CLA refuses the transfer mid-stream. The attacker pays for a few KB of headers; the victim pays nothing for the payload. This is critical for space DTN links where bandwidth is extremely scarce.

If the gate **accepts**: open a spool via `BundleStorage::store()`, push the accumulated header bytes as the first chunk, then forward subsequent CLA chunks through the drain's checks into the spool channel.

With a registered payload peek (`filter_subsystem_design.md`: P > 0), the hook runs once min(P, payload length) payload bytes have accumulated — a bounded extension of the same gate: the header pass pulls segments until they arrive, feeding each through the payload's tail. The peek bytes sit on the invocation side of the spool boundary and are never cached or persisted; an Ingress chain with no peek declared holds none.

### 5.5. Encrypted Bundles and Durability

The node acts as a BPSec **security verifier on ingress and a security acceptor on egress** (RFC 9172 §1.4); where it adds security of its own, it is a security source on egress too. Encrypted or not, a bundle is stored unmodified (§5.6): a BCB-protected payload spools as ciphertext.

- **Ingress and originate — verifier.** The node decrypts, in memory, the BCB-protected extension blocks it needs to process the bundle. These sit in the resident header region and are handled at the pre-drain gate by the KeyProvider-driven machinery (§5.2.1); the stored bytes keep the ciphertext and the BCB. The decoded per-hop values (`previous_node`, `age`, `hop_count`) are cached in the metadata in plaintext, deliberately: they are low-sensitivity, and the node needs them to forward the bundle (expiry, the hop limit). The payload is not decrypted here — the drain carries only the `ValidatingReceiver`'s checks into the spool.
- **Egress and deliver — acceptor.** Where this node is the security acceptor (always for a BCB still present at the bundle's destination, RFC 9172 §5.1.1), the output door decrypts the payload as a streamed cryptographic stage (§6.1) and removes the BCB from the outgoing header region, per attempt.
- **Egress — source.** Where this node adds a BIB or BCB by policy, it stores the bundle as received — plaintext included — and applies the operation at the BPSec-egress seam, per attempt (§6.1.1). The service begins at the source, and the source's own storage is inside its trust boundary: BPSec protects data travelling over the DTN, not the computing resources of the nodes at its ends (RFC 9172 §1). Storage outside the node's trust boundary (an object store or database on another host, removable media) needs encryption at rest for every bundle — plaintext ones this node merely forwards included — which is a property of the storage backend, not a BPSec role.

**AES-GCM streaming decryption**: AES-GCM uses CTR mode internally and can decrypt chunk by chunk. Authentication tag verification is deferred until the final chunk, so the output door withholds the stream's end until the tag verifies: the receiving CLA or Service never sees a complete bundle built from unauthenticated plaintext. On failure the door cancels the stream and discards the bundle — RFC 9172 §5.1.1 requires it for a payload whose ciphertext cannot be authenticated, and the stored ciphertext would fail every later attempt the same way.

**Durability.** In space DTN scenarios, bundle data is extremely precious. Once the CLA receives the last byte, the BPA must not lose it. The spool task writes through to a temp file as data arrives; every byte is on disk as it's written (sequential append). When the producer channel closes, `store()` performs `fsync` + rename and returns, and the bundle is durable. Because the spool holds the bundle as received, ciphertext included, the commit never waits on a key or a tag.

### 5.6. Stored Bundles Are Never Rewritten

A bundle has one stored generation: the bytes as received, held unchanged — encrypted or not (§5.5). Every per-node change is applied per attempt at the output doors and never written back — the scheduled RFC 9172 §5.1.1 removals (`BundleMetadata::to_remove`), the per-hop blocks, a Rewriter's edits, payload decryption where this node is the security acceptor, and any BPSec this node adds at the egress seam.

Restart is simpler for it: `storage_name` always names the one received copy, so recovery has no generation to reconcile and no interrupted rewrite to rerun.

The ADU reassembly partial ([`fragment_reassembly_redesign.md`](fragment_reassembly_redesign.md), not yet built), a bundle this node is assembling rather than one it received, keeps the rule: each fragment merge writes the merged partial as a new object and retires the previous one, so nothing is rewritten in place. The completed bundle is then held unchanged like any other.

### 5.7. Complete Ingress Flow

```
CLA wire
  | transfer segments (or a whole bundle as one Bytes)
  v
Sink::dispatch(stream: &mut dyn Receiver<Segment>) -> Acceptance
  | CLA pushes Segment::Next(bytes); Segment::Final(bytes) ends the bundle;
  | a stream that ends without Final is a truncation (Refused)
  v
Ingest door (process_received_bundle)
  |-- Headers: parse_headers accumulates + parses the primary and
  |     extension blocks (non-canonical CBOR: hard parse error, §5.2.2),
  |     runs keyed BPSec header verification, and begins the deferred
  |     payload-BIB verifiers
  |
  |-- Pre-drain gate
  |     built-in checks (lifetime/hop/expiry, config-gated RFC 9171 validity)
  |     Ingress chain on the resident prefix — Verifiers ∥, then
  |       Classifiers (MetadataDelta: slots, route_table, route_key)
  |     route lookup — the routing decision of record
  |
  |     REJECT / Drop route --> reported drop; nothing spooled or stored
  |                             (Accepted: the transfer itself succeeded)
  |
  |-- Drain (pull-through): the door wraps the arrival in a
  |     ValidatingReceiver (resident head first, then payload segments;
  |     payload CRC, block and outer breaks, trailing data; each payload
  |     chunk fed to the deferred verifiers) and drains it through one
  |     Store::save_stream
  |     failing pull --> nothing saved (truncation / over-cap: Refused;
  |                      invalid content: reported drop)
  |
  |-- Settle: ValidatingReceiver::finish() — deferred payload-BIB verdicts
  |     failure --> the door deletes the staged save; reported drop
  |     (RFC 9172 §5.1.1 failure-drops and deletable-block removals are
  |      scheduled in to_remove, applied at the output doors)
  |
  |-- insert_metadata: the one metadata write, at Dispatching
  |     (the authoritative duplicate check — a duplicate deletes its save)
  |
  '-- execute the gate's routing decision (no dispatch-queue transit)
```

The gate decisions — the Ingress chain, then the routing lookup — settle before a payload byte spools. This ordering is load-bearing, not incidental: a verdict can reject the bundle before any payload is drained, and it is the seat where a Classifier-set storage QoS will shape the save once the policy tranche gives classes one; and the route lookup needs only headers + metadata, so running it at the gate costs the payload nothing while letting an explicit Drop route reject with zero spool I/O.

A structural failure in the streamed bytes (payload CRC, block or outer break, trailing data) fails the pull that carries it, so nothing is saved; a payload-BIB failure can only settle once the whole payload has streamed past, so its verdict is read when the drain returns and the door discards the staged save. This wastes some I/O but is necessary: the BPA has accepted custody from the CLA once the gate passes, so the drain runs to its verdict. A post-gate drop is a decision about a bundle the BPA already owns.

## 6. Egress: Storage to CLA

### 6.1. Egress Byte Processing

A BPA is a router and does not manipulate bundle payloads: where payload inspection or rewriting is needed, a separate CLA or Service (e.g. BIBE) is the pattern, taking the bundle off the hot path rather than adding mangle functionality to it. No registered code touches the byte stream. Egress processes bytes at two fixed points:

- **Header edits** — the per-hop blocks, a Rewriter's extension-block edits, the removals scheduled in `to_remove`, and any BIB or BCB this node adds over a header target — are applied by the Editor to the resident header region before the stream starts. Header blocks are small and held in full.
- **Cryptographic functions** — BPSec integrity and confidentiality over the payload, and payload decryption where this node is the security acceptor (§5.5) — are hard-coded and keyed through the `KeyProvider`. They process the payload incrementally as it streams (§6.1.1).

The egress executor spawns `load()`, emits the edited header region, then streams the stored payload through any cryptographic stage into `Cla::write()`'s channel (§6.2). Without a payload cryptographic stage the payload bytes pass through unchanged, never buffered whole.

#### 6.1.1. Cryptographic Stages

The cryptographic stages compose by encapsulation over the outgoing stream — the edited header region, then the stored payload:

```
Confidentiality stage
  └─ Integrity stage
       └─ (edited header region, then stored payload bytes)
```

Each stage has full structural knowledge before the first byte moves — the `Bundle` index and the resident header region:

- The **Editor** applies the header edits to the resident header region, yielding the updated `Bundle` index.
- The **integrity stage** computes header-target BIBs on the resident header region and inserts them through the Editor. For a payload target it captures the IPPT fields from the primary block and computes the HMAC incrementally as the payload streams past; §6.1.2 covers where that BIB lands.
- The **confidentiality stage** works the same way — AAD from the primary block, header targets encrypted in the resident region, the payload encrypted as it streams (Phase D) — and inserts its BCB.

`Signer` and `Encryptor` stay as bpv7's public whole-bundle utilities — for the `bundle` tool, tests, and any caller holding a bundle in memory. The egress stages reuse them where the targets are resident (header targets) and drive the same incremental primitives (§6.1.3) for the streamed payload, which the whole-bundle utilities do not cover. The Editor likewise remains a general-purpose library component.

The executor touches only the outermost stage: stored bytes in, CLA-ready bytes out. How the stages nest is invisible to it.

#### 6.1.2. Streaming Payload Crypto

Header-target crypto (BIB/BCB on extension blocks) is applied to the resident header region — header blocks are small and held in full.

Payload-target crypto has a **wire-format ordering constraint**: the BIB/BCB is an extension block that must appear before the payload block (RFC 9171 requires payload last), but its content (the HMAC digest or authentication tag) can only be computed after reading the entire payload. This is inherent to BPv7.

**Payload BIB (HMAC) — two-pass at egress.** If the BPSec seam adds a payload BIB at egress, the executor must read the stored bundle twice:

1. *First pass*: stream the payload through the integrity stage to compute the HMAC incrementally (`mac.update()` is push-ready); nothing is emitted.
2. *Second pass*: with the HMAC in hand, the executor emits the header region with the BIB (carrying the HMAC value) inserted, then passes the payload through to the CLA.

For local disk / NOR flash, the second read is essentially free (OS page cache). For S3, it is a second full GET — but this case is narrow (security gateway adding payload BIB at egress).

Computing the HMAC at ingress instead, while the payload streams into the spool, is not done: a security source applies its operations at egress (§5.5), and the stored bundle is never rewritten (§5.6).

**Payload BCB (AES-GCM)** has the same ordering constraint but is deferred to Phase D (security gateway). Decrypting a payload BCB at the output door (§5.5) has no such constraint — removing the BCB is decided before the payload streams — but the tag verifies only at the end, so the door withholds the stream's end until it does. AES-GCM requires a streaming wrapper built on the low-level `aes` + `ghash` crates (§7.3).

#### 6.1.3. BPSec Low-Level API Surface

The BPSec crypto primitives are reusable building blocks for the BPSec machinery. They remain a low-level library, currently in `bpv7/src/bpsec/` (moving to `hardy-bundle` if and when that crate split lands — see §9.1):

**IPPT/AAD construction** — the core of both signing and encryption. Constructs the integrity or authentication input from scope flags, primary block bytes, and target/security block header fields. The construction is a sequence of incremental updates:

```
scope_flags → [primary block bytes] → [target header fields]
  → [security header fields] → target payload bytes
```

Each step is a `mac.update()` or AAD accumulation call. The stage provides these pieces as the bytes stream past — primary block captured early, target block header fields from the `Bundle` index, payload bytes streamed incrementally.

**Crypto operations:**

- `bib_hmac_sha2` — HMAC computation. Already incremental (`hmac` crate's `mac.update()`). Push-ready for the streamed stages.
- `bcb_aes_gcm` — AES-GCM encryption/decryption. Currently requires contiguous buffer (`aes-gcm` crate). Streaming wrapper deferred to Phase D.

**Key management** — `KeySource` trait, `Key` struct, AES key wrapping. Already clean and filter-agnostic.

**Operation result types** — `Parameters`, `Results`, `OperationSet`. These are the CBOR serialization format for BIB/BCB block payloads. The machinery uses them to encode the BIB/BCB data that the Editor inserts.

**Whole-bundle utilities** — `Signer` (add a BIB) and `Encryptor` (add a BCB), built on the primitives above (§6.1.1).

### 6.2. CLA Egress: Cla::forward and Cla::write

> **Landed state (2026-08).** There is no buffered/streamed pair: `Cla::forward` is the one, streamed-only method (keeping `forward`'s `bundle_id` correlation parameter, which the sketch predates), and the buffering adapter the sketch describes is the public helper `stream::buffer_stream` — it buffers the stream via `concat_stream`, enforces `total_len` exactly (with a `usize` pre-flight for 32-bit targets), and maps truncation to `StreamCancelled`, overrun to `PayloadTooLarge`, and short delivery to `PayloadUnderrun`. CLAs that need a contiguous bundle call it explicitly (marked `INTERIM BUFFERING` at each site). The dispatcher forwards the egress rebuild through the streamed door as segments (`dispatcher::output::ChunkReceiver`: the outer array's head, each chunk of the rebuild — new bytes or a zero-copy slice of the resident bytes — and the stored payload streaming on from `Store::load_stream`), with the plan-derived `total_len` below: the rebuilt block index's `encoded_len()`. The egress cryptographic stages and the backends' streamed `load` remain pending.

Original sketch — a buffered `Cla::forward(Bytes)` for CLAs that expect a complete bundle in memory, beside a streaming variant that takes a `Receiver<Segment>` from which the CLA pulls chunks, mirroring the §5.1.3 sketch:

```rust
trait Cla {
    // Existing — full bundle in memory
    async fn forward(
        &self, queue: Option<u32>,
        cla_addr: &ClaAddress,
        data: Bytes,
    ) -> Result<ForwardBundleResult>;

    // New — streaming forward. BPA passes a Receiver<Segment>;
    // CLA pulls Segment::Next(bytes) until Segment::Final (transfer
    // complete, finalise on the wire) or Err(Disconnected) (BPA
    // aborted, tear down any in-flight transfer without delivering
    // a partial bundle).
    async fn write(
        &self, queue: Option<u32>,
        cla_addr: &ClaAddress,
        stream: &dyn Receiver<Segment>,
        total_len: u64,
    ) -> Result<ForwardBundleResult>;
}
```

CLAs that support streaming implement `write()` directly and `forward()` via a small adapter that wraps the input `Bytes` as a single-`Final(bytes)` `Receiver<Segment>`. For non-streaming CLAs, the BPA wraps them in an adapter that collects `Segment::Next` items into a contiguous `Bytes`, appends the `Segment::Final` bytes, then calls `forward()` (or discards the buffer on `Err(Disconnected)`).

Internally, the BPA always uses `write()`. The egress executor reads sequentially from storage, emits the edited header region, streams the payload through any cryptographic stages, and feeds the result into the channel backing the CLA's `Receiver`. For streaming CLAs, bytes flow directly to the wire; for adapted non-streaming CLAs, bytes are collected and forwarded as a single `Bytes` once the stream closes.

`total_len` can be computed from the plan (original bundle size adjusted for block additions/removals). Needed by CLAs that must frame the transfer (e.g., TCPCLv4 XFER_SEGMENT length).

### 6.3. The Common Forward Path

For the hot forward path (no BPSec added at this node, no egress filter edits), no cryptographic stage runs. The output door pulls the stored bundle until its header region is resident, and the attempt's one editor writes the per-hop blocks over the record's block index, from the extension values cached at ingress (§9.3 — the Bundle does not carry decoded extension-block fields). The rebuild's chunks travel to the CLA as segments:

1. The edited header region: the primary block, the updated previous_node (~30B), hop_count (~15B) and bundle_age (~15B) blocks as new bytes, and the remaining extension blocks unchanged, as slices of the resident bytes
2. The stored payload, passed through unchanged: the resident part as a slice, then the rest of the load stream

No crypto. No random access. Sequential read from storage to the CLA. Peak memory: the resident header region plus the read buffer.

### 6.4. Read-Only Storage on the Forward Path

The egress output goes directly to the CLA — it is never written back to storage. The original bundle data remains untouched until `delete()`.

This means:

- **Header segment growth is not a problem.** The Editor may add blocks, the integrity stage may insert BIBs — but the output streams to the CLA, not back to storage.
- **No rewrite of the stored copy.** Stored bytes are never rewritten (§5.6): the forward path is read-once, stream to the CLA, delete.

### 6.5. Failure Handling

If CLA transmission fails, `Cla::write()` returns `Err`; the BPA cancels the producer task feeding its `Receiver`. The original bundle data is still on disk. The bundle stays in its queue for retry. The next attempt re-applies the egress edits and cryptographic stages from scratch over a fresh channel.

## 7. Bundle Struct Reference

### 7.1. Bundle and BundleMetadata

The `Bundle` struct (introduced in §2.3) carries a decoded `PrimaryBlock` alongside the extension-block index:

```rust
pub struct Bundle {
    pub primary: PrimaryBlock,
    pub blocks: HashMap<u64, Block>,
}

pub struct PrimaryBlock {
    pub flags: bundle::Flags,
    pub id: bundle::Id,            // source EID + creation timestamp + fragment info
    pub crc_type: crc::CrcType,
    pub destination: eid::Eid,
    pub report_to: eid::Eid,
    pub lifetime: core::time::Duration,
}
```

Each extension `Block` records structural metadata only: byte extent in the wire data (`Range<u64>`), block type, flags, CRC type, BPSec coverage state (BIB / BCB references), and data range within the block extent. `u64` (rather than `usize`) is used so offsets remain valid on 32-bit targets where bundle storage may exceed `usize::MAX`. There is no `dyn Bundle` trait: the concrete `Bundle` index is the one structural representation — what the parser produces, and what the `Editor` and `ExtensionEditor` take and return. Payload access is abstracted separately, by bpv7's `Reader` trait (a block's header and its payload `Availability`, by block number). Its implementations differ only in where the payload bytes come from: `PlainReader` (the raw wire body of an in-memory bundle), `bpsec::DecryptingReader` (BCB targets decrypted on demand), and the internal overlays of the editor and the BPSec checks (staged rewrites, in-progress decryptions). A block outside the resident bytes reads as `Availability::NotResident` — the headers-only, streaming case.

`BundleMetadata` is the BPA's pipeline-state structure, separate from `Bundle`. It carries the metadata partition of `filter_subsystem_design.md`: provenance / wire cache / classification / infrastructure groups with per-group visibility (`WritableMetadata` and `flow_label` are gone — classification replaced them). `status` is a field of the BPA's bundle record outside the metadata (queue assignment, interim shape), and the resolved next hop rides the `ForwardPending` queue-assignment record. Decoded primary-block fields are read from `Bundle::primary`, never duplicated in metadata.

Clean separation: `bpv7` owns wire-format structure plus the authoritative decoded primary block; `bpa` owns pipeline state and operational semantics.

### 7.2. Parser Output

The parser returns the concrete `Bundle` (decoded `PrimaryBlock` plus extension-block index) along with the pre-parsed BIB and BCB `OperationSet` maps. It validates CBOR structure, decodes the primary block in full, records extension-block extents, and rejects non-canonical encoding as a hard error (`Error::NotCanonical`; see §5.2.2). It does not decode extension-block content beyond what is needed for structural validation. The `RewrittenBundle` enum is eliminated, as is the broader `Checked`/`Rewritten`/`Parsed` taxonomy — see §9.3 for the full collapse of parse modes into streaming primitives + a single in-memory sugar function.

### 7.3. Incremental Crypto Readiness

| Component | Crate | Already Incremental | Streaming Difficulty |
|-----------|-------|--------------------|----|
| CRC-16/32 | `crc` v3 | Yes (`digest.update()`) | **Low** — calling convention change |
| BIB HMAC-SHA2 | `hmac` v0.13 | Yes (`mac.update()`) | **Low** — initialise with headers, push payload |
| BCB AES-GCM | `aes-gcm` v0.11 | No (contiguous only) | **High** — need streaming wrapper or crate swap |

AES-GCM is AES-CTR + GHASH, both inherently streamable. A streaming wrapper built on the low-level `aes` + `ghash` crates is feasible but deferred to the security gateway phase. The confidentiality stage processes payload bytes incrementally as they flow from storage to the CLA, so the wrapper slots in without changing the executor.

## 8. Storage Segmentation and Caching

### 8.1. Headers vs Payload

Bundle data is logically segmented:

- **Header segment**: primary block + all extension blocks (including BIB/BCB). Typically a few hundred bytes to a few KB. Always needed for block-level operations.
- **Payload segment**: payload block data. Variable size, potentially very large. Only needed for delivery, crypto target processing, or verbatim forwarding.

### 8.2. Cache Strategy

With sequential-only storage access, the cache simplifies back to what Hardy already has: an **LRU cache of small bundles**. No layout awareness, no header segment extraction, no split caching strategy.

| Bundle size | Cache strategy |
|------------|----------------|
| Small (< threshold) | LRU cache, take() on load (single refcount, `try_into_mut()`) |
| Large (> threshold) | Not cached; stream from backend on each access |

The cache is populated only on `store()` — never on `load()`. Load takes from the cache (single refcount for in-place mutation). This write-on-store, take-on-load model means the cache acts as a single-use buffer bridging the `store()` → `load()` handoff.

No header segment caching is needed — egress does not require random access to headers. The header region is resident and flows out first, captured as needed.

### 8.3. Backend Considerations

| Backend | Sequential I/O | Notes |
|---------|---------------|-------|
| Local disk | Natural (read/write syscalls) | Optimal for append + sequential scan |
| SSD | Natural | No seek penalty regardless |
| NOR flash (space) | Natural | Sequential read is the native primitive |
| S3 / object store | `PUT` / `GET` | Single request per operation, no range requests needed |

Every backend handles the trait natively without adaptation layers.

Backends differ in preferred **concurrency**, not just I/O pattern: S3 benefits from parallel streams, a single spinning disk wants few and sequential (N concurrent sequential streams degrade to seek thrash), and NOR flash is read-fast/write-slow. Concurrent stream count is therefore a scheduling input, not a backend-internal detail — the backend supplies **directional lane counts** (read lanes / write lanes, each `Option<NonZeroUsize>` with `None` = no preferred limit; in the policy doc's vocabulary a lane is one bounded-quantum service channel, and a resource's lane count is its honest parallelism) to the scheduling layer (`policy_subsystem_redesign.md`, the storage seat).

## 9. Crate Structure

### 9.1. Crate Responsibilities

**`hardy-async`** — channel primitives:

- `channel::Sender` / `channel::Receiver` (bounded channels)

The `Sender<T>` / `Receiver<T>` stream *traits* currently live in **`bpa::stream`**, with blanket impls on the `hardy_async::channel` endpoints in place of adapter types. Promoting the traits into `hardy-async` is a later option if a non-`bpa` consumer needs them; not done today.

**`bpv7`** — wire format, structural indexing, type definitions:

- CBOR encoding/decoding (`FromCbor`, `ToCbor`)
- Block structures, CRC, EID, bundle types (`Block`, `Flags`, `Id`)
- `Bundle` struct — concrete block index (`HashMap<u64, Block>`)
- Parser — wire bytes → `Bundle` index (strict canonical, §5.2.2); streaming `push`/`finish` + `PayloadTail`

**`hardy-bundle`** (optional split, deferred) — bundle manipulation and the cryptographic stages:

- `Editor` — plans against the `Bundle` index, edits the resident header region
- `Span` — internal to the Editor (not exposed)
- `Signer` / `Encryptor` — whole-bundle BIB/BCB utilities
- BPSec low-level crypto APIs (IPPT/AAD construction, HMAC, AES-GCM, key management, operation result types)

The Editor, `Signer`, `Encryptor` and crypto primitives are reusable library components; the BPSec egress stages in the `bpa` crate build on them.

Whether to split `hardy-bundle` from `bpv7` is deferred until the cryptographic stage interfaces stabilise. The Editor is tightly coupled to `bpv7` types; the split becomes a straightforward refactor once the boundary is clear.

**`bpa`** — infrastructure and execution:

- Egress executor — spawns `BundleStorage::load()`, emits the edited header region, streams the payload through any cryptographic stages, feeds `Cla::write()`'s `Receiver`
- Storage traits and backends (`store()` / `load()` / `delete()`)
- Cache (small bundle caching, take semantics)
- Dispatcher, routing, queues, reaper
- CLA/service registries; the streamed `Sink::dispatch()` / `Cla::forward()` surfaces
- `BundleMetadata` — BPA-internal state (provenance, wire cache, classification, infrastructure — the partition in `filter_subsystem_design.md`)
- Filter traits and frozen per-hook chains (`filter_subsystem_design.md` — no byte streams)
- BPSec egress machinery (integrity, confidentiality stages) — uses the Editor on the header region + crypto primitives to produce the cryptographic stages

### 9.2. Dependency Graph

```
hardy-async ← bpv7 ← [hardy-bundle] ← bpa
(Sender,    (wire    (Editor,         (filter chains,
 Receiver,    types,   crypto stages,  BPSec seams,
 channels)     Bundle   BPSec crypto    BundleMetadata,
               index,   primitives)     egress executor,
               Parser)                  storage,
                                        cache,
                                        Sink::dispatch,
                                        Cla::forward)
                            ↑                ↑
                        Services          CLAs
                     (Builder, Editor) (transport only)
```

- `hardy-bundle` (if split) depends on `bpv7` for wire types and `hardy-async` for stream traits, not on `bpa`.
- **Services** depend on `hardy-bundle` (Builder, Editor, types) and `bpa` (Service trait).
- **CLAs** depend on `bpa` (Cla trait, Sink) and `hardy-async` (Receiver for byte streaming) but not on `hardy-bundle` — they are pure transport, delivering wire bytes to the BPA.
- **BPSec machinery** (in `bpa`) uses `hardy-bundle` for the Editor and crypto primitives — built-in pipeline implementations, not separate public APIs.

### 9.3. Library/Application Responsibility Split

The crate boundary follows a single principle: **`bpv7` owns wire-format truth; `bpa` owns operational meaning.** `bpv7` exposes the smallest set of building blocks that lets every consumer assemble what it needs. Consumer-specific helpers — anything that bakes in *how* a particular caller will interpret, route, or report on wire data — live at the call site, not in the library.

**Concrete consequences:**

- **Status reports are for post-parse rejections only.** The streaming parser does not try to recover information from a corrupted bundle — if it fails, the bundle is dropped from the live pipeline (§5.2.2 explains why, and notes the planned quarantine-EID encapsulation capability for post-mortem). Status reports cover bundles whose parse succeeded but were rejected by a filter, policy, or egress failure; in those cases the `PrimaryBlock` is known-valid and the BPA already has the source / report-to EIDs in hand. `bpv7` exposes no best-effort parse helper for corrupt-bundle recovery.

- **Reason-code mapping is BPA policy.** The translation from a filter or policy outcome to a `status_report::ReasonCode` (`BlockUnsupported`, `BlockUnintelligible`, etc.) is a policy decision about how to interpret a rejection for reporting purposes. It lives at the BPA call site, not in `bpv7`. The `status_report::ReasonCode` enum stays in `bpv7` (it's part of the wire format for status report payloads), but the *mapping logic* is the dispatcher's.

- **Decoded extension-block fields live outside `Bundle`.** `Bundle` carries the decoded `PrimaryBlock` plus the structural index of extension blocks only (§7.1). `PreviousNode` / `BundleAge` / `HopCount` are pre-parsed at ingress into `BundleMetadata` (the filter redesign's wire-cache group); anything else is decoded on demand from the block's byte extent. Adding a new extension block type is not a `bpv7` change.

- **There is no parse-mode taxonomy.** Mode variants (canonicalise? drop unsupported blocks? validate BPSec?) would encode dispatcher decisions as parser variants; those decisions belong to pipeline configuration. `bpv7`'s in-memory parse surface is a single sugar function over the streaming primitives, returning the structural `Parsed` for tools, tests, and builder round-trips; production callers compose the primitives (`bpa::bundle::parse`).

- **Reusable pipeline policy lives in `bpa`, not `bpv7`.** The per-hop mutations (hop count, bundle age, previous-node) and validity checks are `bpa` pipeline code against `bpv7`'s lean `Bundle` index. They depend on `bpv7` for wire-format primitives but are not themselves `bpv7` types — which keeps `bpv7` consumable by non-Hardy callers (CLA debug tools, external utilities) without dragging dispatcher policy along.

**The acceptance test** for whether something belongs in `bpv7` or `bpa`: *if BPA needs to query, route, or report by it, it's operational meaning and lives in `bpa`; if it's purely structural (byte extents, CBOR shapes, RFC-defined field decodings), it lives in `bpv7`.* The reason-code mapping policy fails this test — it's BPA's specific need, even though it consumes a `bpv7`-defined enum.

**`bpv7`'s public parse surface** after the refactor is roughly:

```rust
// Streaming primitives (hot path; BPA + tests)
pub use parse::{BundleParser, ParserProgress, Parsed};

// One-shot sugar over the streaming primitives (tools, tests, round-trips)
pub fn parse(data: Bytes) -> Result<Parsed, Error>;   // bpv7::parse::parse
```

Two entry points, each with a one-sentence purpose. `Parsed` is structural (decoded `PrimaryBlock` + extension-block index + decoded BPSec OperationSets); the rich decoded view — extension-block field values — is assembled at the call site (the wire-cache group of `BundleMetadata`, §2.3), not returned by `bpv7`. The reason-code mapping, the filter implementations, and the operational policy that wraps these primitives all live in `bpa`.

## 10. Implementation Phasing

> **Landed state (2026-08).** The pull-side foundations and the streamed door seams, with the service-door twin, are landed — now as the streamed-only `Sink::dispatch` and `ServiceSink::send` after the buffered/streamed pairs collapsed — with the interim whole-buffer accumulation in `bpa::stream::concat_stream` (and the exact-`total_len` `stream::buffer_stream` over it) standing in until Phase A's storage streaming.

Landed work is recorded in the implementation-status block at the top; the phases below cover what remains, in dependency order. The filter subsystem is deliberately absent: filters receive no byte streams (§5.3), so that work proceeds independently (`refactor_plan.md`).

### Phase A: Streaming Storage Write

1. Add the streaming write to `BundleStorage` (§3.1 `store(&dyn Receiver<Segment>)`) across `bundle_mem`, `localdisk`, and `s3`, plus the `CachedBundleStorage` decorator
2. Swap `Store::save_stream`'s RAM accumulator for the backend's streamed write — the one seam (status block)
3. Recovery cleanup for uncommitted spool temp files

Result: the payload-never-in-RAM property (§2.5) on ingress.

### Phase B: Streaming Egress

1. Replace `BundleStorage::load -> Bytes` with the streaming `load(&str, &dyn Sender<Bytes>)` across backends, behind `Store::load_stream`: the output door already pulls only its header region and streams the rest (status block)
2. Retire the `INTERIM BUFFERING` sites: CLAs consume `Cla::forward`'s stream segment-at-a-time rather than calling `stream::buffer_stream` (§6.2)
3. BPSec egress stages (integrity; confidentiality for header targets) as the hard-coded cryptographic stages, keyed through the `KeyProvider` (§6.1.1), reusing `Signer` / `Encryptor` for resident header targets

### Phase C: Tee'd Ingress — dropped

The ingress/originate tee (streaming to the CLA from ingress data before spool commit) was implemented, then removed: the filters decide the bundle's storage priority and behaviour as it is saved, so the drain cannot run in parallel with them — it strictly follows the chain and the route lookup (§5.7). Phase D keeps its letter.

### Phase D: Security Gateway

1. Streaming AES-GCM wrapper for payload BCB (§7.3)
2. Streamed payload decrypt at egress and deliver where this node is the security acceptor, the stream's end withheld until the tag verifies (§5.5)

### Queue integration

Wiring Ingest and ClaSend into the queue model (class-driven FlowControllers) belongs to the queue/policy tranche — see `policy_subsystem_redesign.md`. Two couplings created by this design land there rather than here. **Storage bandwidth becomes a scheduled resource**: once `store()`/`load()` stream (Phases A/B), ingress spooling, egress streaming, and reassembly contend for disk bandwidth — egress drain rate becomes min(link rate, storage read rate) — and the split, most acutely receive-versus-transmit during a bidirectional contact, is a discipline decision, not an accident of task scheduling (the policy doc's "second link" section; the pre-drain gate's ability to decline the payload drain, pushing queueing back into link-layer flow control, is one of that discipline's levers). **The log-jam invariant**: a small high-priority bundle must never wait behind a giant low-priority one, so every shared resource serves at bounded quantum — chunks for byte resources, items for count resources. The per-call channels of §5.1.1 and the sequential spool are what make chunk-quantum service *possible*; the policy tranche's disciplines (and, where a convergence layer cannot interleave an in-flight transfer, per-peer lanes via the `queue` parameter — the lane index, in the policy doc's queue/lane vocabulary) are what make it *hold* end to end.

## 11. Type Safety and Bundle Ownership

RAII falls out naturally from `closeable::Sender<Segment>` (§3.2): an uncommitted spool write is aborted by dropping the producer without first sending `Segment::Final`, and the spool task discards the temp file on `Err(Disconnected)`. No dedicated guard type is needed — the Sender carries the contract, and Drop carries the abort path.

For bundle data and metadata themselves, RAII does not apply: both live in storage backends, the pipeline passes keys (not handles), and `Drop` is synchronous while storage operations are async. An orphaned bundle is bounded by its lifetime field; the reaper expires it, recovery reconciles it.

Typestate within processing blocks (ensuring a bundle passes through required gates before enqueue) is valid in the durable queue model but deferred — each processing block is compact and the machinery cost exceeds the safety benefit at the current codebase size.

## 12. What This Does Not Change

- **MetadataStorage** — bundle identity, status, queue assignment, polling, recovery all remain as-is. The existing `Sender<Bundle>` surface for poll methods is unchanged.
- **BPSec header crypto primitives** — unchanged; BIB/BCB on extension blocks are now driven by the cryptographic stages.
- **Dispatch, FlowController, Deliver, Admin, Reassemble** — these processing blocks work on `BundleMetadata`, not raw bytes.
- **Recovery protocol** — three-phase recovery continues; in-flight spool task temp files (no metadata reference) are cleaned up on startup.
- **Reaper** — operates on expiry indexes in `MetadataStorage`, deleting from `BundleStorage` as ground truth.
- **Bundle data cache** — remains an LRU cache of small bundles (`CachedBundleStorage`); no header-segment caching (§8.2).
