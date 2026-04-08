use hardy_bpv7::status_report::ReasonCode;

/// Initialise all BPA metric descriptions.
///
/// Call once during `Bpa::start()`. Descriptions are registered with the global
/// `metrics` recorder so that OTEL instruments are created with correct units
/// and descriptions on first use.
pub fn init() {
    // -- A. Bundle Reception (CLA ingress) --
    metrics::describe_counter!(
        "bpa.bundle.received",
        metrics::Unit::Count,
        "Bundles received from CLAs"
    );
    metrics::describe_counter!(
        "bpa.bundle.received.bytes",
        metrics::Unit::Bytes,
        "Total bytes of bundle data received from CLAs"
    );
    metrics::describe_counter!(
        "bpa.bundle.received.dropped",
        metrics::Unit::Count,
        "Bundles dropped at reception (invalid CBOR, BPv6, parse failure)"
    );
    metrics::describe_counter!(
        "bpa.bundle.received.duplicate",
        metrics::Unit::Count,
        "Bundles dropped as duplicates at reception"
    );

    // -- B. Bundle Origination (service/app ingress) --
    metrics::describe_counter!(
        "bpa.bundle.originated",
        metrics::Unit::Count,
        "Bundles originated by local services"
    );
    metrics::describe_counter!(
        "bpa.bundle.originated.bytes",
        metrics::Unit::Bytes,
        "Total bytes of bundle data originated by local services"
    );

    // -- C. Bundle Status Gauges --
    metrics::describe_gauge!(
        "bpa.bundle.status",
        metrics::Unit::Count,
        "Live bundles by pipeline state"
    );

    // -- D. Bundle Lifecycle Events --
    metrics::describe_counter!(
        "bpa.bundle.delivered",
        metrics::Unit::Count,
        "Bundles delivered to local services"
    );
    metrics::describe_counter!(
        "bpa.bundle.forwarded",
        metrics::Unit::Count,
        "Bundles forwarded via CLA"
    );
    metrics::describe_counter!(
        "bpa.bundle.forwarding.failed",
        metrics::Unit::Count,
        "Bundle forward attempts that failed (CLA error)"
    );
    metrics::describe_counter!(
        "bpa.bundle.dropped",
        metrics::Unit::Count,
        "Bundles dropped (by reason code)"
    );
    metrics::describe_counter!(
        "bpa.bundle.reassembled",
        metrics::Unit::Count,
        "Fragments successfully reassembled into whole bundles"
    );
    metrics::describe_counter!(
        "bpa.bundle.reassembly.failed",
        metrics::Unit::Count,
        "Reassembly failures (reconstituted bundle invalid)"
    );

    // -- E. Filters --
    metrics::describe_counter!(
        "bpa.filter.filtered",
        metrics::Unit::Count,
        "Bundles dropped by filters (by hook)"
    );
    metrics::describe_counter!(
        "bpa.filter.modified",
        metrics::Unit::Count,
        "Bundles modified by filters (by hook)"
    );
    metrics::describe_counter!(
        "bpa.filter.error",
        metrics::Unit::Count,
        "Filter execution errors (by hook)"
    );

    // -- F. Administrative Records --
    metrics::describe_counter!(
        "bpa.admin_record.received",
        metrics::Unit::Count,
        "Administrative record bundles received"
    );
    metrics::describe_counter!(
        "bpa.admin_record.unknown",
        metrics::Unit::Count,
        "Administrative records that could not be processed"
    );
    metrics::describe_counter!(
        "bpa.status_report.sent",
        metrics::Unit::Count,
        "Status reports sent (by type)"
    );
    metrics::describe_counter!(
        "bpa.status_report.received",
        metrics::Unit::Count,
        "Status reports received (by type)"
    );

    // -- G. Storage --
    metrics::describe_counter!(
        "bpa.store.cache.hits",
        metrics::Unit::Count,
        "Bundle data cache hits"
    );
    metrics::describe_counter!(
        "bpa.store.cache.misses",
        metrics::Unit::Count,
        "Bundle data cache misses"
    );
    metrics::describe_counter!(
        "bpa.store.cache.oversized",
        metrics::Unit::Count,
        "Bundles that bypassed the cache due to size"
    );

    // -- G. In-memory storage backends --
    metrics::describe_gauge!(
        "bpa.mem_store.bundles",
        metrics::Unit::Count,
        "Bundle data entries in memory storage"
    );
    metrics::describe_gauge!(
        "bpa.mem_store.bytes",
        metrics::Unit::Bytes,
        "Bytes used by in-memory bundle storage"
    );
    metrics::describe_counter!(
        "bpa.mem_store.evictions",
        metrics::Unit::Count,
        "LRU evictions from in-memory bundle storage"
    );
    metrics::describe_gauge!(
        "bpa.mem_metadata.entries",
        metrics::Unit::Count,
        "Metadata entries in memory storage"
    );
    metrics::describe_gauge!(
        "bpa.mem_metadata.tombstones",
        metrics::Unit::Count,
        "Tombstone entries in memory metadata storage"
    );

    // -- H. Registries --
    metrics::describe_gauge!(
        "bpa.cla.registered",
        metrics::Unit::Count,
        "Currently registered CLAs"
    );
    metrics::describe_gauge!(
        "bpa.service.registered",
        metrics::Unit::Count,
        "Currently registered services"
    );
    metrics::describe_gauge!(
        "bpa.filter.registered",
        metrics::Unit::Count,
        "Currently registered filters (by hook)"
    );
    metrics::describe_gauge!(
        "bpa.rib.agents",
        metrics::Unit::Count,
        "Currently registered routing agents"
    );
    metrics::describe_gauge!(
        "bpa.rib.entries",
        metrics::Unit::Count,
        "RIB entries from routing agents (by source)"
    );
    metrics::describe_gauge!(
        "bpa.fib.entries",
        metrics::Unit::Count,
        "FIB entries from CLA peers (by CLA)"
    );

    // -- I. Restart/Recovery --
    metrics::describe_counter!(
        "bpa.restart.lost",
        metrics::Unit::Count,
        "Lost bundles discovered during restart"
    );
    metrics::describe_counter!(
        "bpa.restart.duplicate",
        metrics::Unit::Count,
        "Duplicate bundles discovered during restart"
    );
    metrics::describe_counter!(
        "bpa.restart.orphan",
        metrics::Unit::Count,
        "Orphaned bundles discovered during restart"
    );
    metrics::describe_counter!(
        "bpa.restart.junk",
        metrics::Unit::Count,
        "Junk data discovered during restart"
    );
}

/// Convert an optional ReasonCode to a static label string for the `"reason"` label
/// on `bpa.bundle.dropped`.
pub fn reason_label(reason: &ReasonCode) -> &'static str {
    match reason {
        ReasonCode::NoAdditionalInformation => "no_info",
        ReasonCode::LifetimeExpired => "lifetime_expired",
        ReasonCode::ForwardedOverUnidirectionalLink => "unidirectional",
        ReasonCode::TransmissionCanceled => "canceled",
        ReasonCode::DepletedStorage => "depleted_storage",
        ReasonCode::DestinationEndpointIDUnavailable => "dest_unavailable",
        ReasonCode::NoKnownRouteToDestinationFromHere => "no_route",
        ReasonCode::NoTimelyContactWithNextNodeOnRoute => "no_contact",
        ReasonCode::BlockUnintelligible => "block_unintelligible",
        ReasonCode::HopLimitExceeded => "hop_limit",
        ReasonCode::TrafficPared => "traffic_pared",
        ReasonCode::BlockUnsupported => "block_unsupported",
        ReasonCode::MissingSecurityOperation => "missing_security",
        ReasonCode::UnknownSecurityOperation => "unknown_security",
        ReasonCode::UnexpectedSecurityOperation => "unexpected_security",
        ReasonCode::FailedSecurityOperation => "failed_security",
        ReasonCode::ConflictingSecurityOperation => "conflicting_security",
        ReasonCode::Unassigned(_) => "unassigned",
    }
}

/// Convert a BundleStatus to a static label string for the `"state"` label
/// on `bpa.bundle.status`.
pub fn status_label(status: &crate::bundle::BundleStatus) -> &'static str {
    match status {
        crate::bundle::BundleStatus::New => "received",
        crate::bundle::BundleStatus::Dispatching => "dispatching",
        crate::bundle::BundleStatus::ForwardPending { .. } => "forward_pending",
        crate::bundle::BundleStatus::AduFragment { .. } => "fragment",
        crate::bundle::BundleStatus::Waiting => "waiting",
        crate::bundle::BundleStatus::WaitingForService { .. } => "waiting_for_service",
    }
}
