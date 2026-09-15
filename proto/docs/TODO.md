# proto TODO

## BPA-side work the v1 wire is waiting on

The v1 servers are written against a BPA API that is partly still in review. Where a needed call or behaviour is not yet on the BPA, this crate carries a local stand-in rather than weakening the wire contract. Each stand-in below names the BPA change that retires it; none of them is a design choice this crate wants to keep.

### The lane-count bound is a local copy

`MAX_LANE_COUNT` in [`src/lib.rs`](../src/lib.rs) duplicates the BPA's bound on how many egress lanes a CLA may declare. The registration doors enforce it on both sides: the server rejects an out-of-range `lane_count` with `INVALID_ARGUMENT`, and the client SDK refuses it before it reaches the wire, because a silently clamped registration would leave a CLA believing in lanes the BPA never offers.

The BPA holds the same value as a private constant, so the two can drift: raising the BPA's limit without raising this one turns a legal declaration into a wire rejection. When the BPA publishes `cla::MAX_LANE_COUNT`, delete the copy and import it at the two `cla.rs` call sites.

### `services::Error::InvalidSource` cannot name the field at fault

A send whose source EID is not the sending registration's endpoint is a distinct failure, and the status message should name the field at fault. The BPA does not have that variant yet: it reports a spoofed source as `InvalidDestination`, which [`src/server/status.rs`](../src/server/status.rs) turns into an `INVALID_ARGUMENT` naming `SendMetadata.destination`.

When the variant lands, give it an arm in `service_status` whose message names the source rather than the destination. The code stays `INVALID_ARGUMENT`.

### The dispatch acceptance verdict has only one value

`hardy.cla.v1` carries the BPA's acceptance verdict for a dispatched transfer as `DispatchResponse.acceptance`, because the verdict decides whether the CLA may acknowledge the transfer to its peer and the v1 wire is the place that has to be able to say "refused" without inventing a new message later.

The BPA has no verdict to give yet: `cla::Sink::dispatch` returns `Result<()>`, so the server answers `ACCEPTANCE_ACCEPTED` on every completed dispatch and a refusal still travels as a gRPC status (`ABORTED` for a truncated transfer, `RESOURCE_EXHAUSTED` for one over the cap), which the SDK already turns into a failed dispatch. The reading is the same either way: no verdict means refused.

When `cla::Acceptance` lands, return the real verdict from the door in [`src/server/services/cla.rs`](../src/server/services/cla.rs), and map `ACCEPTANCE_REFUSED` onto `Acceptance::Refused` in the SDK sink in [`src/client/services/cla.rs`](../src/client/services/cla.rs), which today can only fail the dispatch with `cla::Error::StreamCancelled`. That matters to the CLA on the other side of the sink: a refusal is one bundle's fate, and a CLA that treats any dispatch failure as a dead link (tcpclv4's session does) drops the peer over it. The typed verdict is what lets a caller tell the two apart.

### Three `services::Error` variants the BPA never constructs

`DtnInvalidServiceName`, `NoIpnNodeId` and `NoDtnNodeId` are dead on the BPA: nothing produces them, and the live node-id failures arrive through `services::Error::NodeId`. `service_status` in [`src/server/status.rs`](../src/server/status.rs) matches them only to stay exhaustive, answering `INVALID_ARGUMENT` for the service name and the same `FAILED_PRECONDITION` for the missing node ids that `NodeId` gets. An unreachable arm should mint no wire contract of its own, and none of the three is an internal failure to be counted as one. Delete the arms when the variants go.

## Deferred tests

Three tests are `#[ignore]`d because the behaviour they assert belongs to the BPA and is not there yet. Each asserts something the v1 wire genuinely promises, so none should be deleted or weakened: un-ignore them with the BPA change named.

| Test | Waiting on |
|------|------------|
| `cla::an_abandoned_forwarding_stays_queued` | the BPA retrying a synchronous `Cla::forward` failure by its kind. An interrupted transfer should re-enter dispatch; today every synchronous error parks the bundle until an unrelated routing event wakes it. |
| `application::re_registration_re_announces_many_parked_deliveries` | per-delivery concurrency in the BPA. Deliveries are serialised per service, so the ack-gated collection window of one delivery blocks that service's next announcement, and a re-registering component sees its parked deliveries one at a time. |
| `lifecycle::deliveries_collect_concurrently` | the same per-delivery concurrency, asserted end to end against a live server: two announced deliveries must be collectable at once. |

## Stall defences that are not settled

`server::watchdog` bounds the stages a client can stall in, each API's semaphore caps how many sessions it can hold at once, and `MAX_INBOUND_TRANSFERS` caps the inbound transfers one session has open. All of it sits above the transport because neither tonic nor hyper offers a per-stream idle timeout or a minimum-rate rule. What follows is what that tier still does not cover.

### Origination has no idempotency key

`SendMetadata` carries no `request_id`, so origination on the application and service APIs is at-least-once: a client whose connection dies between `last_chunk` and the `SendResponse` cannot tell whether the BPA created a bundle, and neither retrying nor not retrying is safe.

A per-session window of recent keys is the shape. The window has a fixed size, and evicting a key a live call still holds is worse than not remembering it at all: the retry it admits originates the second bundle the key exists to prevent, and the late completion of the evicted call can answer that retry with the wrong bundle id. `MAX_INBOUND_TRANSFERS` now bounds how many `Send` calls one session runs at once, so the window has a number to be sized against; what it still needs is a refusal to evict a live claim, and that belongs in the same change as the key.

### The transport knobs are not all configured

`bpa-server/src/grpc.rs` arms `http2_keepalive_interval` and `http2_keepalive_timeout`, which are what notice a half-open connection, since a session with nothing to send never arms an idle bound; it enables the adaptive flow-control window, sets the chunk-sized frame cap, and caps `max_concurrent_streams`. What it does not yet set: `tcp_keepalive`, `concurrency_limit_per_connection` (the session ceiling counts per API, not per connection, so one connection can still open every slot), the initial stream and connection window sizes that bound what a non-reading client can leave buffered when the adaptive window is off, and `http2_max_pending_accept_reset_streams` for Rapid Reset, CVE-2023-44487.

The unary rpcs are bounded by that tier alone. `ReportTransferOutcome`, `AddRoute` and `RemoveRoute` hold no session state, so a drip-fed request body costs a stream and nothing more, and giving them an application-layer bound would duplicate the transport's job rather than extend it.

### The defences have no counters

A stall and a refusal are each a `warn!` and nothing else, so neither can be alerted on or used for capacity planning: there is no count of stalls by stage, no count of subscriptions refused at the ceiling, and no gauge of live sessions per API. The workspace shape for this is settled, so the work is small when it is wanted: a private `otel_metrics` module holding a `describe_*` init, as `tcpclv4` has, with `metrics` as an optional dependency behind the `server` feature so the contract-only build is untouched. The counters pair with state this crate already tracks exactly, the session index insert and remove being the live gauge's two edges.

### A client cannot say "still working"

JetStream's `AckProgress`, SQS's `ChangeMessageVisibility` and Temporal's activity heartbeat all let a client extend a bound it is legitimately still working inside. There is no equivalent here, and probably no need for one: `claim` covers making a single rpc call and `ack` covers confirming bytes the client already holds, so neither spans arbitrary client work. The stage that does span it, a CLA waiting on a forward result, already has the better answer in an `accepted` verdict followed by a later `ReportTransferOutcome`. Revisit only if an operator has to raise `idle` for a whole API to accommodate one slow consumer.

## Possible improvements

### A cancellation-safe BPA registration would collapse the Subscribe task

Each API's `run_session` (in `src/server/services/{endpoint/application,endpoint/service,cla,routing}.rs`) runs on a pool task, and the `subscribe` rpc handler is a proxy that spawns it and waits for its answer on a one-shot. That shape exists for one reason: `register_*` has a commit window it cannot unwind. In `bpa/src/services/registry.rs` the service is published at line 437 and its delivery queue started at 439, but the sink only reaches the caller at 451. Two awaits sit in between, and tonic drops a handler future the moment its rpc dies, so registering inline can leave the BPA holding a published component whose sink was never delivered, which nothing can unregister and whose identity stays in use. A task cannot be cancelled by the client, so the registration always completes and the task then decides whether anyone is still there to receive the stream.

Nothing in that window actually needs the map entry: `rib.add_service` takes the service value and the `Sink` holds a `Weak<Service>`, so only the duplicate check reads the map. The window could become reserve, await, commit: take the lock, reserve the service id, release it, run the awaits, then commit the real entry, with the reservation held by a guard whose `Drop` signals a janitor to run the ordinary unregister path. That makes registration cancellation-safe for every caller rather than for the gRPC servers one at a time.

With that in place each API loses a layer: the spawn and the one-shot go, `run_session` becomes the body of `subscribe`, and the abandoned-rpc branch goes with them. It is a `bpa` change to the commit path every CLA, service and routing registration goes through, so it is its own piece of work, not a follow-up to the wire. `tests/application.rs::an_rpc_abandoned_during_registration_ends_unregistered` is what pins the behaviour either way, and must keep passing across the change.
