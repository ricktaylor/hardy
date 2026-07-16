# bibe TODO

## Back-port BIBE functionality from hardy-file-service

The plan of record is to supersede this crate's BIBE implementation with a back-port from the BIBE work in the hardy-file-service project (Segmented-BIBE PDU framing: shared wire codec, `Segmenter` encap, `Reassembler` decap). That work is still in progress, so neither the shape nor the timing of the back-port is settled; items below are requirements that must hold in whatever implementation lands, not necessarily patches to the current code.

## Delivery failure must not lose the inner bundle (wait-not-drop)

### Background

`DecapService::on_deliver` (`src/service.rs`) swallows a failure of `self.cla.dispatch(inner)` — `warn!` then unconditional `Ok(())`. The dispatcher treats `Ok` as successful delivery, reports delivery, and deletes the outer BIBE bundle; if the inner bundle was never handed to `receive_bundle` (so never persisted), the only copy of the inner data is lost, violating wait-not-drop.

The `Service::on_deliver` contract (added on `fix/concurrent-delivery-stalls`, `bpa/src/services/mod.rs`) makes the fix expressible: returning `Err` parks the outer bundle as `WaitingForService`, and a subsequent registration on the same EID re-delivers it, allowing decapsulation to be retried. Deferred out of that branch's scope; 2026-07-08 review finding #3 (`docs/review_fix-concurrent-delivery-stalls.md`) has the full evidence and trigger analysis.

### What's needed

The code fix landed on `fix/concurrent-delivery-stalls` (commit `3bfe063d`): dispatch failures propagate as `Err` (park + retry), decapsulation failures stay `Ok(())` (permanent, no park). What remains, here or as an acceptance criterion for the back-ported implementation:

- A regression test: deliver a valid BIBE bundle while dispatch is guaranteed to fail (e.g. before the CLA sink registration), then assert the outer bundle is parked rather than deleted, and that it re-delivers once dispatch can succeed. (The generic dispatcher park/re-deliver path is covered by `bpa/tests/pipeline.rs::dispatcher_handles_on_deliver_err`; this test pins the BIBE-specific arm split.)
- Preserve the permanent-vs-transient split through the back-port: only transient dispatch failures may park the outer bundle; permanent decapsulation failures must not.
