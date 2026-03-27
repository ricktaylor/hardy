//! Bundle state machine transitions.
//!
//! This is the **single authoritative location** for every `BundleStatus`
//! change. No other module may call `store.update_status` or `store.watch_bundle`
//! directly. All state transitions go through a named method here.
//!
//! # State Machine
//!
//! ```text
//!  [Receive / Originate]
//!        │
//!        ▼
//!      New ──────────────────────────────────────────► tombstone_with_report (filter / TTL / invalid)
//!        │ ingress filter; checkpoint before routing
//!        ▼
//!   Dispatching ──────────────────────────────────────────────────────────────────────┐
//!        │                                                                             │
//!        ├──► wait_for_route     → Waiting                                            │
//!        │         re-dispatched on RIB update                                        │
//!        │                                                                             │
//!        ├──► wait_for_fragments → AduFragment                                        │
//!        │         all siblings arrived → reassemble → re-enter Dispatching           │
//!        │                                                                             │
//!        ├──► wait_for_service  → WaitingForService                                   │
//!        │         re-dispatched on service registration                              │
//!        │                                                                             │
//!        └──► ForwardPending { peer, queue }  ──► tombstone (CLA acked)               │
//!                  CLA unavailable → wait_for_route ──────────────────────────────────┘
//! ```
//!
//! # `ForwardPending` exception
//!
//! The `Dispatching → ForwardPending` transition is handled inside the egress
//! channel send (see `cla/peers.rs`). This is unavoidable: the queue number is
//! not known until CLA policy classifies the bundle at send time. The transition
//! is documented here for completeness.

use super::*;
use hardy_bpv7::{eid::Eid, status_report::ReasonCode};

impl Dispatcher {
    /// `Dispatching → Waiting`: no route is currently known.
    ///
    /// The bundle will be re-dispatched when the RIB signals a matching route.
    pub(super) async fn wait_for_route(&self, mut bundle: bundle::Bundle) {
        self.store
            .update_status(&mut bundle, bundle::BundleStatus::Waiting)
            .await;
        self.store.watch_bundle(bundle).await;
    }

    /// `Dispatching → AduFragment`: this bundle is a fragment; not all siblings have arrived.
    ///
    /// The bundle will be re-dispatched once all sibling fragments are present.
    pub(super) async fn wait_for_fragments(&self, mut bundle: bundle::Bundle) {
        let status = bundle::BundleStatus::AduFragment {
            source: bundle.bundle.id.source.clone(),
            timestamp: bundle.bundle.id.timestamp.clone(),
        };
        self.store.update_status(&mut bundle, status).await;
        self.store.watch_bundle(bundle).await;
    }

    /// `Dispatching → WaitingForService`: the target service is not yet registered.
    ///
    /// The bundle will be re-dispatched when the service registers with the BPA.
    pub(super) async fn wait_for_service(&self, mut bundle: bundle::Bundle, service: Eid) {
        self.store
            .update_status(
                &mut bundle,
                bundle::BundleStatus::WaitingForService { service },
            )
            .await;
        self.store.watch_bundle(bundle).await;
    }

    /// `* → Tombstone`: send a deletion status report then tombstone.
    pub async fn tombstone_with_report(&self, bundle: bundle::Bundle, reason: ReasonCode) {
        self.report_bundle_deletion(&bundle, reason).await;
        self.tombstone(bundle).await;
    }

    /// `* → Tombstone`: tombstone a bundle without sending a status report.
    ///
    /// Use when no deletion report should be sent: after successful delivery/forwarding
    /// (the relevant report was already sent), or when a filter silently rejects a bundle.
    pub async fn tombstone(&self, bundle: bundle::Bundle) {
        if let Some(storage_name) = &bundle.metadata.storage_name {
            self.store.delete_data(storage_name).await;
        }
        self.store.tombstone_metadata(&bundle.bundle.id).await;
    }
}
