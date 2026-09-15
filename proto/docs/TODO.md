# proto TODO

## BPA-side work the v1 wire is waiting on

The v1 server surfaces are written against a BPA surface that is partly still in review. Where a needed API or behaviour is not yet on the BPA, this crate carries a local stand-in rather than weakening the wire contract. Each stand-in below names the BPA change that retires it; none of them is a design choice this crate wants to keep.

### The lane-count bound is a local copy

`MAX_LANE_COUNT` in [`src/lib.rs`](../src/lib.rs) duplicates the BPA's bound on how many egress lanes a CLA may declare. The registration doors enforce it on both sides: the server rejects an out-of-range `lane_count` with `INVALID_ARGUMENT`, and the client SDK refuses it before it reaches the wire, because a silently clamped registration would leave a CLA believing in lanes the BPA never offers.

The BPA holds the same value as a private constant, so the two can drift: raising the BPA's limit without raising this one turns a legal declaration into a wire rejection. When the BPA publishes `cla::MAX_LANE_COUNT`, delete the copy and import it at the two `cla.rs` call sites.

### `services::Error::InvalidSource` has no wire discriminator

A send whose source EID is not the sending registration's endpoint is a distinct failure, and the wire should carry it as one so the SDK hands the caller the same typed error a local caller gets. The BPA does not have that variant yet: it reports a spoofed source as `InvalidDestination`, which is what the server currently forwards.

When the variant lands, restore the `invalid-source` discriminator in [`src/status.rs`](../src/status.rs): the tag constant, the `embed_service_error` arm, and the `recover_service_error` arm, plus the `INVALID_ARGUMENT` mapping in [`src/server/services/mod.rs`](../src/server/services/mod.rs). Adding a tag is backward compatible; a client that does not know it falls back to the status code.

### The dispatch acceptance verdict has only one value

`hardy.cla.v1` carries the BPA's acceptance verdict for a dispatched transfer as `DispatchResponse.acceptance`, because the verdict decides whether the CLA may acknowledge the transfer to its peer and the v1 wire is the place that has to be able to say "refused" without inventing a new message later.

The BPA has no verdict to give yet: `cla::Sink::dispatch` returns `Result<()>`, so the server answers `ACCEPTANCE_ACCEPTED` on every completed dispatch and a refusal still travels as a gRPC status (`ABORTED` for a truncated transfer, `RESOURCE_EXHAUSTED` for one over the cap), which the SDK already turns into a failed dispatch. The reading is the same either way: no verdict means refused.

When `cla::Acceptance` lands, return the real verdict from the door in [`src/server/services/cla.rs`](../src/server/services/cla.rs), and map `ACCEPTANCE_REFUSED` onto `Acceptance::Refused` in the SDK sink in [`src/client/services/cla.rs`](../src/client/services/cla.rs), which today can only fail the dispatch with `cla::Error::StreamCancelled`. That matters to the CLA on the other side of the sink: a refusal is one bundle's fate, and a CLA that treats any dispatch failure as a dead link (tcpclv4's session does) drops the peer over it. The typed verdict is what lets a caller tell the two apart.

### Three `services::Error` variants the BPA never constructs

`DtnInvalidServiceName`, `NoIpnNodeId` and `NoDtnNodeId` are dead on the BPA: nothing produces them, and the live node-id failures arrive through `services::Error::NodeId`. The server matches them only to stay exhaustive, mapping them to a status code and to no discriminator at all, because an unreachable arm should not mint wire contract and none of the three is an internal failure to be counted as one. Delete both arms when the variants go.

## Deferred tests

Five tests are `#[ignore]`d because the behaviour they assert belongs to the BPA and is not there yet. Each asserts something the v1 wire genuinely promises, so none should be deleted or weakened: un-ignore them with the BPA change named.

| Test | Waiting on |
|------|------------|
| `server::services::cla::tests::a_truncated_dispatch_never_commits` | the BPA committing a streamed ingress bundle only on its final segment. Today a dispatch that stops short of `last_chunk` is still committed, so the door's truncation contract is unenforced. |
| `server::services::cla::tests::a_cancelled_dispatch_is_discarded` | the same gate, for a dispatch cancelled in band rather than truncated. |
| `server::services::cla::tests::an_abandoned_forwarding_stays_queued` | the BPA retrying a synchronous `Cla::forward` failure by its kind. An interrupted transfer should re-enter dispatch; today every synchronous error parks the bundle until an unrelated routing event wakes it. |
| `server::services::application::tests::re_registration_re_announces_many_parked_deliveries` | per-delivery concurrency in the BPA. Deliveries are serialised per service, so the ack-gated collection window of one delivery blocks that service's next announcement, and a re-registering component sees its parked deliveries one at a time. |
| `lifecycle::deliveries_collect_concurrently` | the same per-delivery concurrency, asserted end to end against a live server: two announced deliveries must be collectable at once. |

## Possible improvements

### A cancellation-safe BPA registration would collapse the Subscribe workflow

`Sessions::serve` in [`src/server/subscribe.rs`](../src/server/subscribe.rs) runs on a pool task, and the rpc handler is a proxy that waits for its answer on a one-shot. That shape exists for one reason: `register_*` has a commit window it cannot unwind. In `bpa/src/services/registry.rs` the service is published at line 437 and its delivery queue started at 439, but the sink only reaches the caller at 451. Two awaits sit in between, and tonic drops a handler future the moment its rpc dies, so registering inline can leave the BPA holding a published component whose sink was never delivered, which nothing can unregister and whose identity stays in use. A task cannot be cancelled by the client, so the registration always completes and the workflow then decides whether anyone is still there to receive the stream.

Nothing in that window actually needs the map entry: `rib.add_service` takes the service value and the `Sink` holds a `Weak<Service>`, so only the duplicate check reads the map. The window could become reserve, await, commit: take the lock, reserve the service id, release it, run the awaits, then commit the real entry, with the reservation held by a guard whose `Drop` signals a janitor to run the ordinary unregister path. That makes registration cancellation-safe for every caller rather than for the gRPC surfaces one at a time.

With that in place this crate loses a whole layer: `serve` disappears, `subscribe` becomes inline, the one-shot and the abandoned-rpc branch go, and `handle_requests` is the only spawn. It is a `bpa` change to the commit path every CLA, service and routing registration goes through, so it is its own piece of work, not a follow-up to the wire. `server::services::application::tests::an_rpc_abandoned_during_registration_ends_unregistered` is what pins the behaviour either way, and must keep passing across the change.
