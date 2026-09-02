# Design: Streaming Bundle Pipeline

| Document Info | Details |
| --- | --- |
| **Component** | BPA — Bundle data I/O, spool-based streaming |
| **Scope** | Transformer-based streaming, spool commit model, sequential-only storage |
| **Status** | Living doc — partially implemented (streaming parser, CLA segment delivery, streamed ingress drain, pre-drain gate landed; storage / egress / tee'd spool pending) |
| **Related** | `queue_architecture.md`, `storage_subsystem_design.md`, Editor (`bpv7/src/editor.rs`) |

## Implementation status (2026-08-28)

This is a living document: the sections below mix shipped behaviour with design targets. Current state on the v0.3.0 stack (`refactor/parse` → `refactor/metadata`).

**Landed**

- Streaming parser — `bpv7::parse::BundleParser` (`push` / `finish`), `ParserProgress::{NeedMore, Ready, Partial}`, one-shot sugar `bpv7::parse::parse(Bytes) -> Parsed`. `Partial { consumed, tail: PayloadTail }` is the deferred-payload signal: `push` returns it at the payload boundary and the caller drains the remaining payload through `PayloadTail` (CRC + termination + anti-smuggling checks) without re-buffering headers. Non-canonical CBOR is a hard parse error (§5.2.2). Keyless BPSec structural checks are `bpsec::{bib,bcb}::OperationSet::check`.
- CLA segment delivery — `cla::Segment::{Next, Final}` and the streamed-only `Sink::dispatch(&mut dyn Receiver<Segment>)` (the buffered/streamed trait pairs were collapsed; no `Bytes` variant remains). Stream traits `Sender<T>` / `Receiver<T>` live in `bpa::stream` with adapters over `hardy_async::channel`.
- Streamed ingress drain — `dispatcher::ingress` drives the parser per `Segment` and drains the payload tail via `PayloadTail` (the whole-buffer `concat_stream` path is gone). The tail still accumulates in memory before `save` — that accumulator is the one seam to swap for streaming storage.
- Pre-drain gate — `bundle::parse::parse_headers` + `HeaderVerify::gate_reason`: lifetime/hop/expiry rejection runs on the accumulation buffer before the payload drain (§5.4's early reject in interim form; link-layer reach-back per §1.3 rides the CLA's streaming dispatch path).
- Parse-mode collapse (§9.3) — the three `parse_{preserve,canonicalize,full}_with_provider` pipelines are gone; call sites compose the primitives (`bpa::bundle::parse`).
- Decoded extension fields — pre-parsed at ingress into `BundleMetadata` (§2.3's open choice is resolved; the filter redesign partitions them as the **wire cache** group). The rich `bpa::Bpv7Bundle` view is replaced by structural `hardy_bpv7::Bundle` + metadata.
- `RewrittenBundle` and the `Checked` / `Rewritten` / `Parsed` taxonomy removed from `bpv7`.

**Interim (works, not yet the target shape)**

- `checks::verify_payload`'s contiguous `data: &[u8]` input and `finalize_with_provider`'s whole-buffer parameter are interim: they work only because the drain spools to RAM. When the storage-spool tranche lands, the deferred payload-BIB verification should move into the drain continuation as a streaming digest fed per-segment (the pattern `PayloadTail` already uses for the CRC), deleting `verify_payload` rather than adapting it. Note this carries keyed HMAC state across the drain's awaits — a deliberate departure from the header pass's key-material rule that the spool-tranche design must re-adjudicate explicitly. The deferral seam itself (`HeaderVerify::deferred_bibs`) is the durable half.
- The `Partial`-route drain plumbing is the same class of interim: `parse_headers` hands back the resident `Bytes` plus an `Option<PayloadTail>` the caller must marry to the segment stream, and the accumulator above is what that marriage feeds. The target seam replaces the caller-side marriage with a validating pull-through: bpa wraps the CLA stream in a `Receiver<Segment>` decorator (working name `TailReceiver`) that pulls the inner receiver, feeds the sync `bpv7` `PayloadTail` checks (CRC, termination, anti-smuggling) plus an incremental digest per deferred payload BIB, and yields the same segment onward — so every downstream consumer takes a plain `Receiver<Segment>` (the interim RAM accumulator today, `BundleStorage::store()` after the spool tranche) and any tee composes visibly at the tee site (§5.5's chain in pull form). Layering is deliberate: `PayloadTail` stays no_std, sans-IO and independently fuzzable, and `bpv7` owns the digest framing (the RFC 9173 IPPT's scope-flagged header parts are resident; the block-type-specific payload data streams through), constructed from `HeaderVerify::deferred_bibs`; the async wrapper is bpa's. Captured payload-BCB op-sets are never spent at ingress — they ride the metadata to the §6.1.5 streaming transformers at forward/deliver.

**Pending**

- Streaming storage (§3 — `BundleStorage` still exposes `save(Bytes)` / `load -> Result<Option<Bytes>>`; swapping the ingress tail accumulator for `store()` is the remaining prerequisite for the payload-never-in-RAM property §2.5), egress `Cla::write` + Transformer chain (§6), tee'd spool ingress (§5.7), the §10 phases.

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

The Editor and filter implementations plan against the block index and produce **Transformers** (§6.1) — push-based streaming processors that receive stored bytes and emit transformed bytes. Block data is not accessed via the `Bundle` struct or a trait; it flows through the Transformer at execution time. Full struct details and crypto readiness in §7.

### 2.4. Sequential Write, Sequential Read

Bundle data I/O is fully sequential in both directions:

- **Write (ingress)**: bytes arrive from the CLA in order and are written to storage sequentially. This is a **spool** — data flows through, is committed atomically, and is never modified in place.
- **Read (egress)**: stored bytes are read sequentially and pushed through a Transformer chain. The Transformer captures header data as it flows past (for CRC computation, BPSec IPPT/AAD construction), substitutes or injects modified blocks, and passes through unchanged blocks. No random access is required.

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
| 5 | Storage read | `BundleStorage::load()` reads the whole bundle back | 1GB alloc | remains — §3 / Phase B |
| 6 | Editor flatten | `flatten`/`flatten_inplace()` may need a second buffer | 1-2GB | remains — Transformer (§6.1) / Phase B |
| 7 | CLA forward | `Cla::forward(Bytes)` holds the bundle while transmitting | 1GB resident | remains — `Cla::write` (§6.2) / Phase B |

#### 2.5.2. With This Design

| # | Stage | Change | Memory |
|---|-------|--------|--------|
| 1 | CLA reception | Chunks arrive via channel, no reassembly | Bounded |
| 2 | Parse headers | Streamed parser, headers only | Kilobytes |
| 3 | Pre-drain gate | Runs after headers, before payload | — |
| 4 | Payload to storage | Spools from CLA through pipeline to disk | Bounded |
| 5 | Egress | Stored bytes stream through Transformer to CLA | Bounded |
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

The `store` / `load` names match Hardy's existing storage convention and are unambiguous because the BPA is always the caller. Byte streaming on the CLA egress surface (§6.2) uses `write(&dyn Receiver<Segment>)` — the verb describes what the caller of the method is doing (§4); the ingress method keeps the `dispatch` family (`Sink::dispatch_streamed`).

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

Two operational cases trigger abort:

- **Generational superseding**: a later generation's rewrite completes and commits before an earlier generation's spool write finishes. The earlier generation is now obsolete — its producer drops without sending `Final`, and the spool task discards.
- **Forward-before-store**: the bundle is routed and streamed to the CLA (from the tee'd ingress data, §5.7) before the spool write completes. If the CLA confirms delivery and the bundle is tombstoned, the spool producer drops without committing — the staged data is discarded.

The second case is the hot forward path optimisation: a small bundle tee'd during ingress may be forwarded and acknowledged before its spool write finishes.

## 4. Stream Traits: Sender and Receiver

> **Landed state (2026-08, `refactor/cla-streaming` / `refactor/service-streaming`).** The pull-side foundations of this section are now real, with the landed shape diverging from the sketch below in naming and location, not substance: `Receiver<T>`, `RecvError` (a unit struct), `Segment`, and the interim `concat_stream` (size-capped, truncation-as-error) live in `bpa::stream` rather than `hardy-async`, with blanket impls on the `hardy_async` channel endpoints in place of adapter types. The trait seams are `cla::Sink::dispatch` and `services::ServiceSink::send` (streamed-only since the buffered/streamed pairs were collapsed; a caller holding a whole buffer passes `&mut Bytes` directly — `Bytes` implements `Receiver<Segment>`, draining as a single `Final` segment), not the `write()` naming sketched in this section. A producer death before `Segment::Final` is a truncation error surfaced to the door (`StreamCancelled` — a CLA withholds its transfer ack), reassembly is bounded by `BpaBuilder::max_bundle_size` (`max-bundle-size` in bpa-server config), and registration liveness is enforced per segment by sink-side receiver wrappers. The forward design below is otherwise unchanged and still governs the remaining work.

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

The Sink trait stays; the *implementation* of the lifecycle is the piece that needs rework. The current implementation uses `Weak<RegistryEntry>` plus a `Drop` impl that spawns async cleanup — which is the well-known spawn-from-Drop anti-pattern, duplicates `Weak::upgrade()` boilerplate across each Sink, and creates a "must store the Sink" footgun enforced only by documentation.

The target shape:

- **Ownership.** The component holds `Arc<dyn Sink>` (cheap to clone, methods called directly without `upgrade()` boilerplate). The registry holds a `Weak<dyn Sink>` plus per-component metadata (`ComponentId`, registration time, peer info, etc.).

- **Liveness signal via sync drop, async reconciler.** Each concrete Sink holds a small drop-detector struct whose only job is to push its `ComponentId` onto an `mpsc::UnboundedSender<ComponentId>` when the Sink is dropped. Unbounded because `send()` must never block — it is called from `Drop`. The receiver lives in the registry, drained by a long-lived reconciler task that calls the internal unregistration path. This removes the spawn-from-drop anti-pattern (Drop is plain sync code that pushes one ID and returns), fires exactly once on the last `Arc<dyn Sink>` drop, and composes cleanly with the explicit happy path: if `unregister()` was already called, the reconciler sees a stale ID and no-ops.

- **Explicit `unregister()` as the documented happy path.** `sink.unregister().await` removes the component from the registry under its mutex, awaits any in-flight method's completion, and returns. The component may still be holding its `Arc<dyn Sink>` afterwards; subsequent calls return `Disconnected` because the Sink's internal `alive: AtomicBool` (in an `Arc<Inner>` shared between Sink and registry) is now false. No `Weak::upgrade()` per method — one atomic load is enough.

- **BPA-initiated shutdown.** `registry.shutdown()` iterates entries, flips each Sink's `alive` flag, calls `component.on_unregister()` (async, sequenced), and removes the entry. Components see `Disconnected` on subsequent calls; the component's own later drop of the `Arc<dyn Sink>` fires the reconciler signal which finds nothing to do.

- **The "must store the Sink" footgun inverts.** Components either call `unregister()` explicitly or simply drop their `Arc<dyn Sink>` when done; both paths converge cleanly and the silent unregistration on drop becomes a feature rather than a side-effect of the only liveness mechanism. The Sink stays alive as long as anything holds a clone.

- **Proto crate impact is minimal.** The remote Sink impl in proto already holds its own state and lifecycle; the same drop-detector pattern lives inside it, with the reconciler closing the gRPC stream when the local-side Sink is dropped. The split reader/writer `RpcProxy` is largely unchanged.

The reconciler loop uses `Closeable` (§4) rather than a hand-rolled `select_biased!(cancel | recv)` shape. The same lifecycle design applies uniformly to `ServiceSink`, `ApplicationSink`, and `RoutingSink` — one pattern across all registries.

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

If the BPA rejects mid-stream (a gate rejection), `dispatch_streamed()` returns early; the CLA's pushing task sees `SendError` on its next push and tears down the wire transfer (e.g., emitting XFER_REFUSE on TCPCLv4).

The BPA core only implements the streaming ingress path. (Implemented as the streamed-only `Sink::dispatch`; the §4 `write`/`read` naming convention was not adopted for this ingress method — it kept the `dispatch` family. The buffered `dispatch(Bytes, ..)` convenience existed as a provided default while the pair coexisted, and was removed when the pairs collapsed — callers pass whole buffers directly, `Bytes` being a `Receiver<Segment>`. Egress/filter surface naming is still open.)

**Transitional convenience.** Retaining both `dispatch()` / `dispatch_streamed()` (and likewise `Cla::forward()` / `Cla::write()` in §6.2) is transitional. Every existing CLA reads from a network stream and artificially materialises the full bundle before calling the non-streaming variant; all would benefit from migrating. Once migration is complete, the non-streaming methods can be removed.

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

    /// Phase 1: push each incoming chunk. Returns `NeedMore(n)` to
    /// request more bytes, `Ready(bytes)` once the bundle's structure
    /// has been fully walked (`bytes` = the full concatenation, ready
    /// to relay into the spool in one piece), or `Partial` for an
    /// oversized payload (drain via `PayloadTail`).
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

Splitting structural walk (`push`) from BPSec validation (`finish`) lets the caller hand `Ready(bytes)` to the spool immediately while `finish()` runs in parallel — the BPSec structural pass overlaps with the first spool write rather than serialising.

#### 5.2.1. BPSec Verification at the Gate

**Keyed** BPSec operations (BIB hash verification, BCB decryption / tag verification) are **not** performed by the parser. They run at the pre-drain gate as fixed, KeyProvider-driven machinery, keeping key material and security policy out of the parser.

**Keyless** structural validation of BPSec blocks **is** performed by the parser. RFC 9172 defines a set of inter-block structural rules — BCB MUST NOT target the primary block (§3.7), BCB MUST NOT target another BCB (§3.7), BIB MUST NOT target a BCB (§3.9), each block at most one BCB target (§3.9), BCBs MUST NOT set `delete_block_on_failure` (§3.7), BCBs targeting the payload MUST set `must_replicate` (§3.7), security operations unique per `(service, target)` (§2.6), every targeted block number must exist in the bundle — that are pure functions of the parsed `Bundle` index plus the Abstract Security Block contents of each BIB/BCB. No keys, no policy, no I/O.

The parser handles BIBs and BCBs asymmetrically during the walk (zero cost for bundles with no security blocks):

- **BCBs** are decoded inline during the block walk. The ASB (which describes what the BCB encrypts) is itself plaintext, so the parser parses each BCB's `OperationSet` as it sees it.
- **BIBs** are recorded by block number in a pending list and decoded by `finish()`. The deferral is necessary because a BIB may itself be a BCB target — in which case its body is ciphertext until the BCB is decrypted. `finish()` parses every BIB whose body is plaintext; BIBs whose bodies are BCB-protected are skipped (their target blocks get `BibCoverage::Maybe`, deferred to the keyed verification step).

`finish()` returns the `Bundle` index along with the pre-parsed BIB and BCB `OperationSet` maps, so downstream filters don't re-decode. The cross-block rules are applied here; on violation, `finish()` returns `Err` and the BPA aborts the spool write that ran in parallel. This is the earliest a structurally-malformed bundle can be rejected. The structural validators are exposed as `bpsec::{bib,bcb}::OperationSet::check` (composed by `bpv7::checks`) so offline tooling can run the same checks without standing up a filter pipeline.

The keyed BPSec verification step (after the parser, with key access) has access to the accumulation buffer (all header blocks in memory) and the `Bundle` block index for structural navigation. Header-block BIBs are verified immediately. Payload-block BIBs require payload data — they are verified inline as the payload spools past (the deferred-BIB tee, §5.7).

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

Payload bytes that arrived after the parser completed continue to stream as subsequent chunks. This means that during egress, the Transformer (§6.1) receives header blocks first, allowing it to capture header data before the payload arrives.

The `Span` model is **not needed at ingress** — it is internal to the Editor's Transformer (§6.1). At ingress, the accumulation buffer is mutated only by the fixed finalize step; the parser is pure and can be tested independently.

### 5.3. Filters in the Streaming Pipeline

Filter design — kinds, hooks, registration, metadata, restart re-admission — is owned by [`filter_subsystem_design.md`](filter_subsystem_design.md). What matters to the byte pipeline is the shape of the contract: all three kinds (parallel **Verifier**; sequential **Classifier** at the input hooks; extension-block **Rewriter** at the output hooks) are payload-free, and each boundary has exactly one single-pass hook — Ingress on the pre-drain gate (§5.4), Originate pre-store, Egress in ClaSend between the per-hop rewrite and the BPSec seam, Deliver before payload decrypt.

The byte contract keeps filters off the streaming path entirely:

- An invocation receives `(&Bundle, data: &[u8])` — the resident header prefix plus, when a registered Classifier declared a payload peek, the first min(P, payload length) payload bytes. Block bodies are read through `bpv7`'s existing accessors (`Block::payload` / `Block::extract`), which return `None` for bytes not resident in `data`.
- No filter receives a byte stream, holds a stream open, or blocks the drain: the hook runs on the accumulation buffer at the gate, and the payload spools past untouched.
- Classifiers return a `MetadataDelta` (`class`, `route_key`, registered annotation slots) that the engine applies; the class drives the dispatch enqueue. Filters never mutate stored bytes — the egress Rewriter edits extension blocks per transmission attempt, in memory, so §6.4's read-only forward path holds by construction.
- The originate-raw path (`local_dispatch_raw()`, bundles from services via gRPC) runs the same strict parser → gate pipeline as ingress — non-canonical service-provided bytes are rejected at parse (§5.2.2), never canonicalised.

Built-in checks (validity, rfc9171 strictness) are pipeline code gated by `Config`, upstream of any registered filter; BPSec verification and the §5.1.1 failure-drop are fixed machinery (§5.2.1), never filters.

### 5.4. The Pre-Drain Gate

After all header blocks are parsed and before the payload arrives, the pre-drain gate runs: built-in checks, BPSec header verification, then the Ingress hook (Verifiers in parallel, then Classifiers).

If the gate **rejects** (a built-in check, or a registered Ingress Verifier per `filter_subsystem_design.md`): the BPA returns from `Sink::dispatch_streamed()` without commencing a spool. The CLA's pushing task sees `SendError` on its next push and can cancel the transfer (e.g., TCPCLv4 XFER_REFUSE, UDPCLv2 stops accepting datagrams). For a 1GB payload from a rejected source, the BPA has received only the header blocks. Zero wasted I/O.

This is the mechanism behind the **link-layer-reach** motivator (§1.3): the BPA's reject decision propagates back to the wire, the CLA cancels the transfer mid-stream, and the link layer reclaims its resources without ever delivering the payload bytes. It is also effectively **DDoS protection**. Without the gate, a DTN node is trivially DoS-able: an attacker sends oversized bundles with forged sources, and the victim must receive, parse, store, and process the entire payload before deciding to reject. With it, the BPA inspects headers (~hundreds of bytes), rejects, and the CLA refuses the transfer mid-stream. The attacker pays for a few KB of headers; the victim pays nothing for the payload. This is critical for space DTN links where bandwidth is extremely scarce.

If the gate **accepts**: open a spool via `BundleStorage::store()`, push the accumulated header bytes as the first chunk, then forward subsequent CLA chunks through any configured transforms into the spool channel.

With a registered payload peek (`filter_subsystem_design.md`: P > 0), the hook runs once min(P, payload length) payload bytes have accumulated — a bounded extension of the same gate; the peek bytes sit on the invocation side of the spool boundary and are never cached or persisted.

### 5.5. Inline Payload Transforms and Durability

Security policy (fixed machinery, not a registered filter) may configure a transform on the payload stream. The primary use case is **security gateway payload decryption**.

Ingress payload transforms use Transformers (§6.1) — the same push-based model used for egress. CRC verification uses a Verifier (§6.1.2) — the same push-based consumer model. These compose sequentially:

```
CLA chunks -> [CRC Verifier] -> [BPSec decrypt Transformer] -> spool channel
```

**AES-GCM streaming decryption**: AES-GCM uses CTR mode internally and can decrypt chunk by chunk. Authentication tag verification is deferred until the final chunk. The spool must not be committed until tag verification succeeds; on failure the spool task is cancelled via its token and discards the staged data.

**Durability.** In space DTN scenarios, bundle data is extremely precious. Once the CLA receives the last byte, the BPA must not lose it.

- *Normal case (no payload transform)*: the spool task writes through to a temp file as data arrives. Every byte is on disk as it's written (sequential append). When the producer channel closes, `store()` performs `fsync` + rename and returns. The bundle is durable.
- *Transform case (payload decryption)*: the spool contains decrypted data. The producer side withholds channel close until tag verification. If the BPA crashes before verification, the decrypted spool is discarded on recovery (temp file, no metadata pointing to it). The original encrypted bundle must be retransmitted. This is inherent — you cannot commit unverified data.

### 5.6. Generational Rewrites

Bundle data has at most two generations during ingress: the original wire bytes, and — only when fixed post-drain machinery must rewrite (e.g. the ingress-time payload-BIB insertion, §6.1.5) — a second generation produced by streaming generation 0 through the rewrite into a new spool.

```
Generation 0: original wire-format bytes from CLA
  committed during Phase B (spool from CLA, §5.7)

  → load(gen0) → Receiver<Bytes>
  → fixed rewrite machinery (e.g. payload-BIB insertion)
  → store() → new spool

Generation 1: rewritten output
  committed (fsync + rename)
  metadata.update(storage_name = gen1)
  delete(gen0)
```

Registered filters never drive a generational rewrite — they do not mutate stored bytes (§5.3). Only the final output is committed as a new generation.

**Crash recovery**: the metadata's `storage_name` always points to the last successfully committed generation. If the BPA crashes during the rewrite, the in-flight `store()` task's temp file has no metadata reference — cleaned up on recovery. The last committed generation is intact; recovery reruns the rewrite from it.

In the common case there is nothing to rewrite and generation 0 is the final generation.

### 5.7. Complete Ingress Flow

```
CLA wire
  | transfer segments (or complete Bytes via dispatch())
  v
sink.dispatch_streamed(stream: &dyn Receiver<Segment>, ...)
  | CLA pushes Segment::Next(bytes) into the channel feeding the Receiver
  | CLA sends Segment::Final(bytes) at end of bundle; drop without Final = abort
  v
Ingest
  |-- Phase A:
  |     accumulate + parse primary block
  |     accumulate + parse extension blocks
  |     (non-canonical CBOR: hard parse error, §5.2.2)
  |
  |-- Pre-drain gate + Ingress hook — Verifiers ∥, then Classifiers
  |     built-in checks (lifetime/hop/expiry) + BPSec header verification
  |     Classifiers apply the MetadataDelta (class, route_key, slots)
  |     (a declared payload peek waits for min(P, payload len) bytes)
  |
  |     REJECT --> return from Sink::dispatch_streamed()
  |               (zero I/O, no storage, no metadata)
  |
  |     ACCEPT --> spawn BundleStorage::store(spool_stream)
  |               push complete header segment as first Segment::Next
  |               MetadataStorage::store()
  |               start routing lookup (async, needs only metadata)
  |
  |-- Phase B (tee'd):
  |     payload chunks pulled from the CLA stream are tee'd:
  |       ├→ spool_sender.send(chunk)  [feeds store() task]
  |       └→ payload-BIB verification  [fixed pipeline: deferred_bibs,
  |             incremental HMAC as the bytes stream past]
  |
  |-- on CLA stream closed (end of bundle):
  |     drop spool_sender → store() returns storage_name (fsync + rename)
  |     Finalize (fixed): deferred payload-BIB results,
  |       §5.1.1 failure-drop / removal rewrites
  '--   enqueue(dispatch; class from the Classifier chain)
        routing result available (started during Phase B)
```

Routing lookup begins as soon as the gate accepts and metadata is stored — it needs only `BundleMetadata` (destination, class), not payload data. The lookup runs concurrently with payload spooling; by the time the drain finalizes and the bundle is ready for dispatch, the routing result is typically already available.

A payload-BIB failure detected during spooling cancels the spool task (token signalled). This wastes some I/O but is necessary: the BPA has accepted custody from the CLA once the gate passes, so spooling must begin immediately. A post-gate drop is a decision about a bundle the BPA already owns.

## 6. Egress: Storage to CLA

### 6.1. The Transformer Model

The egress path is driven by **Transformers** — push-based streaming processors that consume stored bundle bytes sequentially and emit transformed bytes. Transformers are produced by the Editor and the BPSec egress stages during a planning phase that inspects the `Bundle` block index, then executed by the BPA pushing stored bytes through.

**Caveat.** The Transformer model is the current proposal but the specific shape is provisional — the specific closure signature may evolve as the egress executor is built. The load-bearing properties (push-based, chunk-driven, no full-bundle materialisation, composable without random access) are what the rest of the design depends on; the precise interface is not.

#### 6.1.1. Transformer Interface

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

- `Some(bytes)` — push input data. Returns `NeedMore` or `Emit(Vec<Bytes>)`.
- `None` — signal end of input. The Transformer flushes any buffered state and returns `Done(Vec<Bytes>, Bundle)`.
- `Err` at any point — the input stream is invalid; the caller abandons the stream and the spool (if applicable).
- The Transformer always consumes all input. `Done` only appears in response to `None`. A Transformer that detects a problem mid-stream returns `Err`, not `Done`.

The `Done` variant returns the updated `Bundle` block index, reflecting any blocks that were added, removed, or modified.

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

#### 6.1.2. Verifier: Push-Based Consumer

The Verifier follows the same push-based input model but is a **consumer**, not a transform — it validates data without producing output bytes.

```rust
type Verifier = Box<dyn FnMut(Option<Bytes>) -> VerifyResult>;

pub enum VerifyResult {
    NeedMore,
    Done(Result<()>),
}
```

Same `Option<Bytes>` input contract. The Verifier captures primary block fields and target block content as bytes flow through, then computes and checks signatures. For header-block BIBs, verification completes as soon as the target block has passed. For payload-block BIBs, the Verifier incrementally computes HMAC over the payload and verifies on `None`.

Use cases:

- **Ingress gate**: Verifier pushed the accumulation buffer, validates header-block BIBs against in-memory header data.
- **Payload BIB**: Verifier runs alongside ingress spooling, receiving the same bytes that flow to storage.
- **Composed stages**: a stage can run a Verifier alongside a Transformer, feeding the same input to both; the executor checks the Verifier's result before committing.

#### 6.1.3. Editor Produces a Transformer

The Editor plans against the concrete `Bundle` struct — block numbers, types, flags, byte extents. The existing `Vec<Span>` (`Unchanged(Range)` / `New(Box<[u8]>)`) becomes **internal state** of the Transformer closure, not an exposed data structure. The Transformer is a state machine that:

1. Tracks byte position in the input stream
2. At each block boundary, decides: pass through (unchanged block), substitute (new header bytes, skip original), or inject (emit new block bytes at this position)
3. Emits output bytes as they become available
4. On `None`, returns the updated `Bundle` index

The planning phase does not need `source_data` — it only inspects the block index. The Transformer receives the source bytes at execution time. The `'a` lifetime is removed from the Editor; `Cow<'a, [u8]>` in `BlockTemplate` is replaced with owned bytes for caller-provided data and positional references for existing block data (resolved by the Transformer as bytes flow through).

#### 6.1.4. Chained Transformers

Transformers compose by encapsulation. An outer stage wraps an inner stage's Transformer, processing its output:

```
Confidentiality stage
  └─ Integrity stage
       └─ Editor's Transformer
            └─ (receives stored bytes)
```

Each stage has full structural knowledge at planning time:

- **Editor** plans against the original `Bundle` index, produces a Transformer and an updated `Bundle` index reflecting its modifications.
- The **integrity stage** uses the Editor to insert a BIB block, then wraps the Editor's Transformer. The wrapping Transformer:
  - Pushes input bytes into the Editor's Transformer
  - Receives the Editor's `Emit` outputs
  - Captures IPPT fields (primary block data) as they flow past
  - Buffers BIB target blocks, computes HMAC incrementally
  - Injects BIB blocks at the correct position
  - Emits its own output
- The **confidentiality stage** wraps similarly — captures AAD from the primary block, buffers BCB target blocks, encrypts, and injects BCB blocks.

The `Signer` and `Encryptor` as standalone public types are **eliminated**. Their orchestration logic dissolves into the BPSec egress machinery (the integrity and confidentiality stages). The Editor remains as a general-purpose library component; BPSec crypto primitives remain as reusable low-level APIs (§6.1.6).

The outermost Transformer is the only thing the executor touches. Stored bytes flow in, CLA-ready bytes flow out. The layered planning and execution is invisible to the caller.

#### 6.1.5. Streaming Payload Crypto

Header-target crypto (BIB/BCB on extension blocks) is handled naturally by the Transformer chain — header blocks are small and captured in full as they flow through.

Payload-target crypto has a **wire-format ordering constraint**: the BIB/BCB is an extension block that must appear before the payload block (RFC 9171 requires payload last), but its content (the HMAC digest or authentication tag) can only be computed after reading the entire payload. This is inherent to BPv7.

**Payload BIB (HMAC) — two-pass at egress.** If the BPSec seam adds a payload BIB at egress, the executor must read the stored bundle twice:

1. *First pass*: stream the payload through the Transformer to compute the HMAC incrementally (`mac.update()` is push-ready). The Transformer accumulates IPPT header fields and the HMAC digest but emits no output.
2. *Second pass*: the Transformer now has the HMAC result. It emits header blocks, the BIB (with HMAC value), then passes through the payload to the CLA.

For local disk / NOR flash, the second read is essentially free (OS page cache). For S3, it is a second full GET — but this case is narrow (security gateway adding payload BIB at egress).

**Payload BIB — preferred: compute at ingress.** The cleaner approach is to compute the payload BIB at **ingress time**, when the payload bytes are already streaming past:

- The security gateway's ingress policy identifies bundles that need a payload BIB.
- A Verifier-like consumer runs alongside payload spooling, computing the HMAC incrementally as bytes flow to storage.
- The BIB is added by a post-drain generational rewrite (§5.6) after the HMAC is complete.
- At egress, the BIB is already stored — no extra work.

This avoids the two-pass problem entirely. The HMAC is computed once, during the single ingress pass, and durably stored.

**Payload BCB (AES-GCM)** has the same ordering constraint but is deferred to Phase 3 (security gateway). AES-GCM requires a streaming wrapper built on the low-level `aes` + `ghash` crates (§7.3).

#### 6.1.6. BPSec Low-Level API Surface

The BPSec crypto primitives are reusable building blocks for the BPSec machinery. They remain a low-level library, currently in `bpv7/src/bpsec/` (moving to `hardy-bundle` if and when that crate split lands — see §9.1):

**IPPT/AAD construction** — the core of both signing and encryption. Constructs the integrity or authentication input from scope flags, primary block bytes, and target/security block header fields. The construction is a sequence of incremental updates:

```
scope_flags → [primary block bytes] → [target header fields]
  → [security header fields] → target payload bytes
```

Each step is a `mac.update()` or AAD accumulation call. The stage provides these pieces as they flow through the Transformer — primary block captured early, target block header fields from the `Bundle` index, payload bytes streamed incrementally.

**Crypto operations:**

- `bib_hmac_sha2` — HMAC computation. Already incremental (`hmac` crate's `mac.update()`). Push-ready for Transformers.
- `bcb_aes_gcm` — AES-GCM encryption/decryption. Currently requires contiguous buffer (`aes-gcm` crate). Streaming wrapper deferred to Phase 3.

**Key management** — `KeySource` trait, `Key` struct, AES key wrapping. Already clean and filter-agnostic.

**Operation result types** — `Parameters`, `Results`, `OperationSet`. These are the CBOR serialization format for BIB/BCB block payloads. The machinery uses them to encode the BIB/BCB data that the Editor inserts.

**What is removed:**

- `BlockSet` trait — replaced by the Transformer's internal state capturing block data as it streams past
- `Signer` struct — orchestration dissolves into the integrity stage
- `Encryptor` struct — orchestration dissolves into the confidentiality stage
- `EditorBlockSet` — no longer needed without `BlockSet`

### 6.2. CLA Egress: Cla::forward and Cla::write

> **Landed state (2026-08, `feat/cla-forward-streamed`, then collapsed).** The streaming variant first landed as `Cla::forward_streamed` alongside the buffered `forward(Bytes)`, with a buffering default adapter bridging the pair. The pair has since been collapsed: `Cla::forward` is now the streamed-only method (keeping `forward`'s `bundle_id` correlation parameter, which the sketch predates), and the adapter body became the public helper `stream::buffer_stream` — it buffers the stream via `concat_stream`, enforces `total_len` exactly (with a `usize` pre-flight for 32-bit targets), and maps truncation to `StreamCancelled`, overrun to `PayloadTooLarge`, and short delivery to `PayloadUnderrun`. CLAs that need a contiguous bundle call it explicitly (marked `INTERIM BUFFERING` at each site). The dispatcher always forwards through the streamed door, passing the loaded bundle as `&mut Bytes` (a whole buffer is itself a one-segment `Receiver`) with `total_len = data.len()`. The egress executor, streamed storage `load`, and the Transformer-derived `total_len` below remain pending.

The existing `Cla::forward(Bytes)` method is retained for CLAs that expect a complete bundle in memory. A new streaming variant takes a `Receiver<Segment>` from which the CLA pulls chunks, mirroring `Sink::dispatch_streamed` from §5.1:

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

Internally, the BPA always uses `write()`. The egress executor reads sequentially from storage, pushes bytes through the Transformer chain, and feeds the Transformer's output into the channel backing the CLA's `Receiver`. For streaming CLAs, bytes flow directly to the wire; for adapted non-streaming CLAs, bytes are collected and forwarded as a single `Bytes` once the stream closes.

`total_len` can be computed from the Transformer's plan knowledge (original bundle size adjusted for block additions/removals). Needed by CLAs that must frame the transfer (e.g., TCPCLv4 XFER_SEGMENT length). Migration to streaming-only is transitional (§5.1).

### 6.3. The Common Forward Path

For the hot forward path (no BPSec added at this node, no egress filter mutations), the Transformer is a simple state machine. It identifies blocks by `block_type` in the `Bundle` index (§9.3 — the Bundle does not carry decoded extension-block fields), decodes the body from the streamed bytes as they flow past, mutates, and re-emits:

1. Receive primary block bytes → emit rewritten primary (~50B)
2. Receive previous_node block → decode body, update, emit (~30B)
3. Receive hop_count block → decode body, update, emit (~15B)
4. Receive bundle_age block → decode body, update, emit (~15B)
5. Receive remaining extension blocks → pass through unchanged
6. Receive payload → pass through unchanged

No crypto. No random access. Sequential read from storage, through the Transformer, to the CLA. Peak memory: the read buffer plus ~110 bytes of new header blocks.

### 6.4. Read-Only Storage on the Forward Path

The Transformer's output goes directly to the CLA — it is never written back to storage. The original bundle data remains untouched until `delete()`.

This means:

- **Header segment growth is not a problem.** The Editor may add blocks, the integrity stage may insert BIBs — but the output streams to the CLA, not back to storage.
- **No generational rewrite for forwarding.** Generational saves (§5.6) are only for the fixed post-drain rewrites. The forward path is read-once, stream through Transformer, delete.

### 6.5. Failure Handling

If CLA transmission fails, `Cla::write()` returns `Err`; the BPA cancels the producer task feeding its `Receiver`. The original bundle data is still on disk. The bundle stays in its queue for retry. The next attempt constructs a fresh Transformer and channel pair and re-applies egress mutations from scratch.

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

Each extension `Block` records structural metadata only: byte extent in the wire data (`Range<u64>`), block type, flags, CRC type, BPSec coverage state (BIB / BCB references), and data range within the block extent. `u64` (rather than `usize`) is used so offsets remain valid on 32-bit targets where bundle storage may exceed `usize::MAX`. There is no `dyn Bundle` trait, no `BlockSet` trait, and no multiple implementations — a single concrete representation is used everywhere (parser output, Editor input, Transformer output).

`BundleMetadata` is the BPA's pipeline-state structure, separate from `Bundle`. It carries the metadata partition of `filter_subsystem_design.md`: provenance / wire cache / classification / infrastructure groups with per-group visibility (`WritableMetadata` and `flow_label` are gone — classification replaced them). `status` is a field of the BPA's bundle record outside the metadata (queue assignment, interim shape), and the resolved next hop rides the `ForwardPending` queue-assignment record. Decoded primary-block fields are read from `Bundle::primary`, never duplicated in metadata.

Clean separation: `bpv7` owns wire-format structure plus the authoritative decoded primary block; `bpa` owns pipeline state and operational semantics.

### 7.2. Parser Output

The parser returns the concrete `Bundle` (decoded `PrimaryBlock` plus extension-block index) along with the pre-parsed BIB and BCB `OperationSet` maps. It validates CBOR structure, decodes the primary block in full, records extension-block extents, and rejects non-canonical encoding as a hard error (`Error::NotCanonical`; see §5.2.2). It does not decode extension-block content beyond what is needed for structural validation. The `RewrittenBundle` enum is eliminated, as is the broader `Checked`/`Rewritten`/`Parsed` taxonomy — see §9.3 for the full collapse of parse modes into streaming primitives + a single in-memory sugar function.

### 7.3. Incremental Crypto Readiness

| Component | Crate | Already Incremental | Streaming Difficulty |
|-----------|-------|--------------------|----|
| CRC-16/32 | `crc` v3 | Yes (`digest.update()`) | **Low** — calling convention change |
| BIB HMAC-SHA2 | `hmac` v0.13 | Yes (`mac.update()`) | **Low** — initialise with headers, push payload |
| BCB AES-GCM | `aes-gcm` v0.10 | No (contiguous only) | **High** — need streaming wrapper or crate swap |

AES-GCM is AES-CTR + GHASH, both inherently streamable. A streaming wrapper built on the low-level `aes` + `ghash` crates is feasible but deferred to the security gateway phase. The Transformer model makes this straightforward — the confidentiality stage's Transformer processes payload bytes incrementally as they flow through.

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

The cache is populated only on `store()` / `replace()` — never on `load()`. Load takes from the cache (single refcount for in-place mutation). This write-on-store, take-on-load model means the cache acts as a single-use buffer bridging the `store()` → `load()` handoff.

No header segment caching is needed — the Transformer model does not require random access to headers. Headers flow through the Transformer sequentially, captured as needed.

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

The `Sender<T>` / `Receiver<T>` stream *traits* and their channel adapters (`ChannelSender<T>`, `ChannelReceiver<T>`) currently live in **`bpa::stream`**, wrapping `hardy_async::channel`. Promoting the traits into `hardy-async` is a later option if a non-`bpa` consumer needs them; not done today.

**`bpv7`** — wire format, structural indexing, type definitions:

- CBOR encoding/decoding (`FromCbor`, `ToCbor`)
- Block structures, CRC, EID, bundle types (`Block`, `Flags`, `Id`)
- `Bundle` struct — concrete block index (`HashMap<u64, Block>`)
- Parser — wire bytes → `Bundle` index (strict canonical, §5.2.2); streaming `push`/`finish` + `PayloadTail`

**`hardy-bundle`** (optional split, deferred) — bundle manipulation and Transformer production:

- `Transformer` type and `TransformResult` enum
- `Verifier` type and `VerifyResult` enum
- `Editor` — plans against `Bundle` index, produces `Transformer`
- `Span` — internal to Editor's Transformer (not exposed)
- BPSec low-level crypto APIs (IPPT/AAD construction, HMAC, AES-GCM, key management, operation result types)

The `Signer` and `Encryptor` structs are eliminated; their orchestration logic moves into the BPSec egress stages in the `bpa` crate. The Editor and crypto primitives remain as reusable library components.

Whether to split `hardy-bundle` from `bpv7` is deferred until the Transformer interfaces stabilise. The Editor is tightly coupled to `bpv7` types; the split becomes a straightforward refactor once the boundary is clear.

**`bpa`** — infrastructure and execution:

- Egress executor — spawns `BundleStorage::load()`, drives the Transformer chain, feeds `Cla::write()`'s `Receiver`
- Storage traits and backends (`store()` / `load()` / `delete()`)
- Cache (small bundle caching, take semantics)
- Dispatcher, routing, queues, reaper
- CLA/service registries; `Sink::dispatch_streamed()` / `Cla::write()` surfaces
- `BundleMetadata` — BPA-internal state (provenance, wire cache, classification, infrastructure — the partition in `filter_subsystem_design.md`)
- Filter traits and frozen per-hook chains (`filter_subsystem_design.md` — payload-free, no byte streams)
- BPSec egress machinery (integrity, confidentiality stages) — uses Editor + crypto primitives to produce Transformers and Verifiers
- Generational rewrite executor (loads stored bundle, pipes through the fixed rewrite machinery into a new `store()` call)

### 9.2. Dependency Graph

```
hardy-async ← bpv7 ← [hardy-bundle] ← bpa
(Sender,    (wire    (Editor,         (filter chains,
 Receiver,    types,   Transformer,    BPSec seams,
 channels)     Bundle   Verifier,       BundleMetadata,
               index,   BPSec crypto    egress executor,
               Parser)  primitives)     storage,
                                        cache,
                                        Sink::dispatch_streamed,
                                        Cla::write)
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

Two entry points, each with a one-sentence purpose. `Parsed` is structural (decoded `PrimaryBlock` + extension-block index + decoded BPSec OperationSets); the rich decoded view — extension-block field values — is assembled at the call site (today: `bpa::Bpv7Bundle`), not returned by `bpv7`. The reason-code mapping, the filter implementations, and the operational policy that wraps these primitives all live in `bpa`.

## 10. Implementation Phasing

> **Landed state (2026-08).** Phase 1's pull-side foundations and the door seams from Phase 3 item 2 plus the service-door twin are landed — now as the streamed-only `Sink::dispatch` and `ServiceSink::send` after the buffered/streamed pairs collapsed — with the interim whole-buffer accumulation in `bpa::stream::concat_stream` (and the exact-`total_len` `stream::buffer_stream` over it) standing in until Phase 2's storage streaming. The remaining items below are unstarted.

Landed work is recorded in the implementation-status block at the top; the phases below cover what remains, in dependency order. The filter subsystem is deliberately absent: filters receive no byte streams (§5.3), so that work proceeds independently (`refactor_plan.md`).

### Phase A: Streaming Storage Write

1. Add the streaming write to `BundleStorage` (§3.1 `store(&dyn Receiver<Segment>)`, or an interim `save_stream`) across `bundle_mem`, `localdisk`, and `sqlite`, plus the `store.rs` wrapper
2. Swap the ingress drain's in-memory tail accumulator for the spool — the one seam (status block)
3. Recovery cleanup for uncommitted spool temp files

Result: the payload-never-in-RAM property (§2.5) on ingress.

### Phase B: Transformer and Streaming Egress

1. Define `Transformer` / `TransformResult` and `Verifier` / `VerifyResult` (§6.1); refactor the Editor to plan against the block index and produce a Transformer (`Vec<Span>` becomes closure state; `source_data` and the `'a` lifetime go; `flatten()` retained for tests during transition)
2. Replace `BundleStorage::load -> Bytes` with the streaming `load(&str, &dyn Sender<Bytes>)` across backends
3. Add `Cla::write(&dyn Receiver<Segment>, total_len)`; an adapter wraps `forward()` for non-streaming CLAs
4. Egress executor: spawn `load()`, drive the Transformer chain, feed `Cla::write()`'s channel (§6.1.1)
5. BPSec egress stages (integrity; confidentiality for header targets) as wrapping Transformers (§6.1.4); `Signer` / `Encryptor` orchestration dissolves into them

### Phase C: Tee'd Ingress

1. Tee payload chunks to the deferred payload-BIB verifier during spooling (§5.7)
2. Hot-forward tee: stream to the CLA from ingress data before spool commit (§3.2)

### Phase D: Security Gateway

1. Streaming AES-GCM wrapper for payload BCB (§7.3)
2. Inline decrypt Transformer at ingress; deferred commit for unverified payloads (§5.5)

### Queue integration

Wiring Ingest and ClaSend into the queue model (class-driven FlowControllers) belongs to the queue/policy tranche — see `policy_subsystem_redesign.md`. Two couplings created by this design land there rather than here. **Storage bandwidth becomes a scheduled resource**: once `store()`/`load()` stream (Phases A/B), ingress spooling, egress streaming, generational rewrites, and reassembly contend for disk bandwidth — egress drain rate becomes min(link rate, storage read rate) — and the split, most acutely receive-versus-transmit during a bidirectional contact, is a discipline decision, not an accident of task scheduling (the policy doc's "second link" section; the pre-drain gate's ability to decline the payload drain, pushing queueing back into link-layer flow control, is one of that discipline's levers). **The log-jam invariant**: a small high-priority bundle must never wait behind a giant low-priority one, so every shared resource serves at bounded quantum — chunks for byte resources, items for count resources. The per-call channels of §5.1.1 and the sequential spool are what make chunk-quantum service *possible*; the policy tranche's disciplines (and, where a convergence layer cannot interleave an in-flight transfer, per-peer lanes via the `queue` parameter — the lane index, in the policy doc's queue/lane vocabulary) are what make it *hold* end to end.

## 11. Type Safety and Bundle Ownership

RAII falls out naturally from `closeable::Sender<Segment>` (§3.2): an uncommitted spool write is aborted by dropping the producer without first sending `Segment::Final`, and the spool task discards the temp file on `Err(Disconnected)`. No dedicated guard type is needed — the Sender carries the contract, and Drop carries the abort path.

For bundle data and metadata themselves, RAII does not apply: both live in storage backends, the pipeline passes keys (not handles), and `Drop` is synchronous while storage operations are async. An orphaned bundle is bounded by its lifetime field; the reaper expires it, recovery reconciles it.

Typestate within processing blocks (ensuring a bundle passes through required gates before enqueue) is valid in the durable queue model but deferred — each processing block is compact and the machinery cost exceeds the safety benefit at the current codebase size.

## 12. What This Does Not Change

- **MetadataStorage** — bundle identity, status, queue assignment, polling, recovery all remain as-is. The existing `Sender<Bundle>` surface for poll methods is unchanged.
- **BPSec header crypto primitives** — unchanged; BIB/BCB on extension blocks are now driven by the Transformer chain.
- **Dispatch, EgressController, Deliver, Admin, Reassemble** — these processing blocks work on `BundleMetadata`, not raw bytes.
- **Recovery protocol** — three-phase recovery continues; in-flight spool task temp files (no metadata reference) are cleaned up on startup.
- **Reaper** — operates on expiry indexes in `MetadataStorage`, deleting from `BundleStorage` as ground truth.
- **Bundle data cache** — remains an LRU cache of small bundles (`CachedBundleStorage`); no header-segment caching (§8.2).
