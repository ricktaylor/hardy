/*!
Domain-error transport over gRPC `Status` metadata.

A status code alone cannot round-trip the BPA's domain error enums, so
the server attaches a discriminator to the status metadata:
[`KIND_KEY`] carries a kebab-case tag naming the variant, and
[`DETAIL_KEY`] carries the variant's payload, when it has one. The
server attaches with the `embed_*` functions; the client SDK reads
back with the `recover_*` functions.

Recovery is best-effort: an unknown kind or an unparseable payload
recovers as `None`, and the caller falls back to its status-code
mapping. The codes themselves are mapped in each side's `*_status` and
`*_error` functions, not here.
*/

use hardy_bpa::{cla, node_ids, routing, services};
use hardy_bpv7::{eid::Eid, status_report::ReasonCode};
use tonic::{Status, metadata::MetadataValue};

/// Metadata key carrying the tag that names the domain error variant.
pub const KIND_KEY: &str = "hardy-error-kind";

/// Metadata key carrying the variant's payload, for kinds that have
/// one.
pub const DETAIL_KEY: &str = "hardy-error-detail";

// Discriminator tags. These are v1 wire contract: renaming one breaks
// existing peers.
const ALREADY_EXISTS: &str = "already-exists";
const DISCONNECTED: &str = "disconnected";
const DROPPED: &str = "dropped";
const DUPLICATE_BUNDLE: &str = "duplicate-bundle";
const INTERNAL: &str = "internal";
const INVALID_BUNDLE: &str = "invalid-bundle";
const INVALID_DESTINATION: &str = "invalid-destination";
const NODE_ID: &str = "node-id";
const NULL_NEXT_HOP: &str = "null-next-hop";
const PAYLOAD_TOO_LARGE: &str = "payload-too-large";
const PAYLOAD_UNADDRESSABLE: &str = "payload-unaddressable";
const PAYLOAD_UNDERRUN: &str = "payload-underrun";
const SERVICE_ID_IN_USE: &str = "service-id-in-use";
const ADMINISTRATIVE_ENDPOINT: &str = "administrative-endpoint";
const STREAM_CANCELLED: &str = "stream-cancelled";
const VIA_OWN_NODE: &str = "via-own-node";

// Detail tags for the `node-id` kind.
const NODE_ID_LOCAL_NODE: &str = "local-node";
const NODE_ID_MULTIPLE_IPN: &str = "multiple-ipn-node-ids";
const NODE_ID_MULTIPLE_DTN: &str = "multiple-dtn-node-ids";
const NODE_ID_NO_IPN: &str = "no-ipn-node-id";
const NODE_ID_NO_DTN: &str = "no-dtn-node-id";
const NODE_ID_INVALID_EID: &str = "invalid-eid";

// A detail that is not valid gRPC ASCII metadata is silently dropped;
// recovery then returns `None` and the caller falls back to its code
// mapping.
fn attach(mut status: Status, kind: &'static str, detail: Option<String>) -> Status {
    let metadata = status.metadata_mut();
    metadata.insert(KIND_KEY, MetadataValue::from_static(kind));
    if let Some(value) = detail.and_then(|d| MetadataValue::try_from(d).ok()) {
        metadata.insert(DETAIL_KEY, value);
    }
    status
}

fn kind_of(status: &Status) -> Option<&str> {
    status
        .metadata()
        .get(KIND_KEY)
        .and_then(|v| v.to_str().ok())
}

fn detail_of(status: &Status) -> Option<&str> {
    status
        .metadata()
        .get(DETAIL_KEY)
        .and_then(|v| v.to_str().ok())
}

fn encode_pair(a: u64, b: u64) -> String {
    format!("{a} {b}")
}

fn decode_pair(detail: &str) -> Option<(u64, u64)> {
    let (a, b) = detail.split_once(' ')?;
    Some((a.parse().ok()?, b.parse().ok()?))
}

/// Attaches the discriminator for a services error to a built status.
pub fn embed_service_error(status: Status, e: &services::Error) -> Status {
    let (kind, detail) = match e {
        services::Error::ServiceIdInUse(id) => (SERVICE_ID_IN_USE, Some(id.clone())),
        services::Error::AdministrativeEndpoint(id) => (ADMINISTRATIVE_ENDPOINT, Some(id.clone())),
        services::Error::Disconnected => (DISCONNECTED, None),
        services::Error::PayloadTooLarge { size, max } => {
            (PAYLOAD_TOO_LARGE, Some(encode_pair(*size, *max)))
        }
        services::Error::PayloadUnderrun { size, expected } => {
            (PAYLOAD_UNDERRUN, Some(encode_pair(*size, *expected)))
        }
        services::Error::PayloadUnaddressable { total_len } => {
            (PAYLOAD_UNADDRESSABLE, Some(total_len.to_string()))
        }
        services::Error::NodeId(e) => (NODE_ID, Some(node_id_to_tag(e).to_string())),
        services::Error::InvalidDestination(eid) => (INVALID_DESTINATION, Some(eid.to_string())),
        services::Error::StreamCancelled => (STREAM_CANCELLED, None),
        services::Error::Dropped(reason) => (
            DROPPED,
            reason
                .map(u64::from)
                .filter(|code| ReasonCode::try_from(*code).is_ok())
                .map(|code| code.to_string()),
        ),
        services::Error::DuplicateBundle => (DUPLICATE_BUNDLE, None),
        services::Error::InvalidBundle(_) => (INVALID_BUNDLE, None),
        services::Error::Internal(_) => (INTERNAL, None),
        // The BPA never constructs these three variants (see this
        // crate's `docs/TODO.md`). They get no discriminator, so the
        // client classifies them by status code alone. Delete this arm
        // with the variants.
        services::Error::DtnInvalidServiceName(_)
        | services::Error::NoIpnNodeId
        | services::Error::NoDtnNodeId => return status,
    };
    attach(status, kind, detail)
}

/// Returns the services error a status discriminates, or `None` when
/// the kind or payload cannot be recovered.
pub fn recover_service_error(status: &Status) -> Option<services::Error> {
    Some(match kind_of(status)? {
        SERVICE_ID_IN_USE => services::Error::ServiceIdInUse(detail_of(status)?.to_string()),
        ADMINISTRATIVE_ENDPOINT => {
            services::Error::AdministrativeEndpoint(detail_of(status)?.to_string())
        }
        DISCONNECTED => services::Error::Disconnected,
        PAYLOAD_TOO_LARGE => {
            let (size, max) = decode_pair(detail_of(status)?)?;
            services::Error::PayloadTooLarge { size, max }
        }
        PAYLOAD_UNDERRUN => {
            let (size, expected) = decode_pair(detail_of(status)?)?;
            services::Error::PayloadUnderrun { size, expected }
        }
        PAYLOAD_UNADDRESSABLE => services::Error::PayloadUnaddressable {
            total_len: detail_of(status)?.parse().ok()?,
        },
        NODE_ID => services::Error::NodeId(node_id_from_tag(detail_of(status)?)?),
        INVALID_DESTINATION => {
            services::Error::InvalidDestination(detail_of(status)?.parse::<Eid>().ok()?)
        }
        STREAM_CANCELLED => services::Error::StreamCancelled,
        DROPPED => services::Error::Dropped(match detail_of(status) {
            None => None,
            Some(code) => Some(ReasonCode::try_from(code.parse::<u64>().ok()?).ok()?),
        }),
        DUPLICATE_BUNDLE => services::Error::DuplicateBundle,
        _ => return None,
    })
}

/// Attaches the discriminator for a CLA error to a built status.
pub fn embed_cla_error(status: Status, e: &cla::Error) -> Status {
    let (kind, detail) = match e {
        cla::Error::AlreadyExists(name) => (ALREADY_EXISTS, Some(name.clone())),
        cla::Error::Disconnected => (DISCONNECTED, None),
        cla::Error::StreamCancelled => (STREAM_CANCELLED, None),
        cla::Error::PayloadTooLarge { size, max } => {
            (PAYLOAD_TOO_LARGE, Some(encode_pair(*size, *max)))
        }
        cla::Error::PayloadUnderrun { size, expected } => {
            (PAYLOAD_UNDERRUN, Some(encode_pair(*size, *expected)))
        }
        cla::Error::PayloadUnaddressable { total_len } => {
            (PAYLOAD_UNADDRESSABLE, Some(total_len.to_string()))
        }
        cla::Error::Internal(_) => (INTERNAL, None),
    };
    attach(status, kind, detail)
}

/// Returns the CLA error a status discriminates, or `None` when the
/// kind or payload cannot be recovered.
pub fn recover_cla_error(status: &Status) -> Option<cla::Error> {
    Some(match kind_of(status)? {
        ALREADY_EXISTS => cla::Error::AlreadyExists(detail_of(status)?.to_string()),
        DISCONNECTED => cla::Error::Disconnected,
        STREAM_CANCELLED => cla::Error::StreamCancelled,
        PAYLOAD_TOO_LARGE => {
            let (size, max) = decode_pair(detail_of(status)?)?;
            cla::Error::PayloadTooLarge { size, max }
        }
        PAYLOAD_UNDERRUN => {
            let (size, expected) = decode_pair(detail_of(status)?)?;
            cla::Error::PayloadUnderrun { size, expected }
        }
        PAYLOAD_UNADDRESSABLE => cla::Error::PayloadUnaddressable {
            total_len: detail_of(status)?.parse().ok()?,
        },
        _ => return None,
    })
}

/// Attaches the discriminator for a routing error to a built status.
pub fn embed_routing_error(status: Status, e: &routing::agent::Error) -> Status {
    let (kind, detail) = match e {
        routing::agent::Error::AlreadyExists(name) => (ALREADY_EXISTS, Some(name.clone())),
        routing::agent::Error::Disconnected => (DISCONNECTED, None),
        routing::agent::Error::NullNextHop => (NULL_NEXT_HOP, None),
        routing::agent::Error::ViaOwnNode(eid) => (VIA_OWN_NODE, Some(eid.to_string())),
        routing::agent::Error::Internal(_) => (INTERNAL, None),
    };
    attach(status, kind, detail)
}

/// Returns the routing error a status discriminates, or `None` when
/// the kind or payload cannot be recovered.
pub fn recover_routing_error(status: &Status) -> Option<routing::agent::Error> {
    Some(match kind_of(status)? {
        ALREADY_EXISTS => routing::agent::Error::AlreadyExists(detail_of(status)?.to_string()),
        DISCONNECTED => routing::agent::Error::Disconnected,
        NULL_NEXT_HOP => routing::agent::Error::NullNextHop,
        VIA_OWN_NODE => routing::agent::Error::ViaOwnNode(detail_of(status)?.parse::<Eid>().ok()?),
        _ => return None,
    })
}

fn node_id_to_tag(e: &node_ids::Error) -> &'static str {
    match e {
        node_ids::Error::LocalNode => NODE_ID_LOCAL_NODE,
        node_ids::Error::MultipleIpnNodeIds => NODE_ID_MULTIPLE_IPN,
        node_ids::Error::MultipleDtnNodeIds => NODE_ID_MULTIPLE_DTN,
        node_ids::Error::NoIpnNodeId => NODE_ID_NO_IPN,
        node_ids::Error::NoDtnNodeId => NODE_ID_NO_DTN,
        node_ids::Error::InvalidEid(_) => NODE_ID_INVALID_EID,
    }
}

// No arm for `invalid-eid`: its nested parse error cannot be
// reconstructed, so that kind recovers as `None`.
fn node_id_from_tag(tag: &str) -> Option<node_ids::Error> {
    Some(match tag {
        NODE_ID_LOCAL_NODE => node_ids::Error::LocalNode,
        NODE_ID_MULTIPLE_IPN => node_ids::Error::MultipleIpnNodeIds,
        NODE_ID_MULTIPLE_DTN => node_ids::Error::MultipleDtnNodeIds,
        NODE_ID_NO_IPN => node_ids::Error::NoIpnNodeId,
        NODE_ID_NO_DTN => node_ids::Error::NoDtnNodeId,
        _ => return None,
    })
}
