/*!
The typed-error half of the wire contract.

Domain errors cross the wire as a gRPC `Status`, whose code is the
coarse, convention-correct classification and whose message is the
human-readable rendering. Codes alone cannot round-trip the domain
enums (several variants share `InvalidArgument`), so the server also
attaches a machine-readable discriminator to the status metadata:

- [`KIND_KEY`]: a stable kebab-case tag naming the domain variant.
- [`DETAIL_KEY`]: the variant's payload, in a compact per-kind
  encoding (an EID string, decimal sizes), when it has one.

The `embed_*`/`recover_*` pairs below are the whole contract: the
server bridges embed on their error path, the client SDK recovers in
its shared status mapping, and both sides compiling against this one
module keeps the tag tables in lockstep. Recovery is best-effort by
design: a kind whose payload cannot travel (`invalid-bundle` and
`internal` wrap arbitrary nested errors; a `node-id` failure wrapping
a malformed EID) recovers as `None`, and the caller falls back to its
status-code mapping. A foreign (non-SDK) client can ignore the
metadata entirely; the codes and messages are unchanged.
*/

use hardy_bpa::{cla, node_ids, routing, services};
use hardy_bpv7::{eid::Eid, status_report::ReasonCode};
use tonic::{Status, metadata::MetadataValue};

/// Metadata key carrying the domain-error discriminator tag.
pub const KIND_KEY: &str = "hardy-error-kind";

/// Metadata key carrying the discriminated variant's payload, when the
/// kind defines one.
pub const DETAIL_KEY: &str = "hardy-error-detail";

// The discriminator tags. Stable wire contract: renaming one is a
// breaking change to the v1 wire.
const ALREADY_EXISTS: &str = "already-exists";
const DISCONNECTED: &str = "disconnected";
const DROPPED: &str = "dropped";
const DUPLICATE_BUNDLE: &str = "duplicate-bundle";
const INTERNAL: &str = "internal";
const INVALID_BUNDLE: &str = "invalid-bundle";
const INVALID_DESTINATION: &str = "invalid-destination";
const INVALID_SOURCE: &str = "invalid-source";
const NODE_ID: &str = "node-id";
const NULL_NEXT_HOP: &str = "null-next-hop";
const PAYLOAD_TOO_LARGE: &str = "payload-too-large";
const PAYLOAD_UNADDRESSABLE: &str = "payload-unaddressable";
const PAYLOAD_UNDERRUN: &str = "payload-underrun";
const SERVICE_ID_IN_USE: &str = "service-id-in-use";
const STREAM_CANCELLED: &str = "stream-cancelled";
const VIA_OWN_NODE: &str = "via-own-node";

// The nested tags of a `node-id` failure's detail.
const NODE_ID_LOCAL_NODE: &str = "local-node";
const NODE_ID_NULL_ENDPOINT: &str = "null-endpoint";
const NODE_ID_DTN_WITH_DEMUX: &str = "dtn-with-demux";
const NODE_ID_MULTIPLE_IPN: &str = "multiple-ipn-node-ids";
const NODE_ID_MULTIPLE_DTN: &str = "multiple-dtn-node-ids";
const NODE_ID_NO_IPN: &str = "no-ipn-node-id";
const NODE_ID_NO_DTN: &str = "no-dtn-node-id";
const NODE_ID_INVALID_EID: &str = "invalid-eid";

// Attaches a discriminator to a built status. A detail that does not
// fit gRPC's ASCII metadata is dropped rather than failing the status:
// recovery then falls back to the code mapping, which is the same
// experience a foreign client always has.
fn attach(status: Status, kind: &'static str, detail: Option<String>) -> Status {
    let mut status = status;
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

// "size max" / "size expected" pairs travel as two decimal u64s; a
// value that does not fit the receiving target's usize fails recovery.
fn encode_pair(a: usize, b: usize) -> String {
    format!("{a} {b}")
}

fn decode_pair(detail: &str) -> Option<(usize, usize)> {
    let (a, b) = detail.split_once(' ')?;
    Some((
        a.parse::<u64>().ok()?.try_into().ok()?,
        b.parse::<u64>().ok()?.try_into().ok()?,
    ))
}

/// Attaches the discriminator for a services-surface error to its
/// built status.
pub fn embed_service_error(status: Status, e: &services::Error) -> Status {
    let (kind, detail) = match e {
        services::Error::ServiceIdInUse(id) => (SERVICE_ID_IN_USE, Some(id.clone())),
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
        services::Error::NodeId(e) => (NODE_ID, Some(node_id_tag(e).to_string())),
        services::Error::InvalidDestination(eid) => (INVALID_DESTINATION, Some(eid.to_string())),
        services::Error::InvalidSource(eid) => (INVALID_SOURCE, Some(eid.to_string())),
        services::Error::StreamCancelled => (STREAM_CANCELLED, None),
        services::Error::Dropped(reason) => (DROPPED, reason.map(|r| u64::from(r).to_string())),
        services::Error::DuplicateBundle => (DUPLICATE_BUNDLE, None),
        services::Error::InvalidBundle(_) => (INVALID_BUNDLE, None),
        services::Error::Internal(_) => (INTERNAL, None),
    };
    attach(status, kind, detail)
}

/// The services-surface error a status discriminates, when its kind and
/// payload both recover; `None` falls back to the caller's code mapping.
pub fn recover_service_error(status: &Status) -> Option<services::Error> {
    Some(match kind_of(status)? {
        SERVICE_ID_IN_USE => services::Error::ServiceIdInUse(detail_of(status)?.to_string()),
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
        INVALID_SOURCE => services::Error::InvalidSource(detail_of(status)?.parse::<Eid>().ok()?),
        STREAM_CANCELLED => services::Error::StreamCancelled,
        DROPPED => services::Error::Dropped(match detail_of(status) {
            None => None,
            // An unassigned reason code round-trips as `Unassigned` rather
            // than failing recovery, matching the wire route-action decode.
            Some(code) => {
                let code = code.parse::<u64>().ok()?;
                Some(ReasonCode::try_from(code).unwrap_or(ReasonCode::Unassigned(code)))
            }
        }),
        DUPLICATE_BUNDLE => services::Error::DuplicateBundle,
        _ => return None,
    })
}

/// Attaches the discriminator for a CLA-surface error to its built
/// status.
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

/// The CLA-surface error a status discriminates, when it recovers.
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

/// Attaches the discriminator for a routing-surface error to its built
/// status.
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

/// The routing-surface error a status discriminates, when it recovers.
pub fn recover_routing_error(status: &Status) -> Option<routing::agent::Error> {
    Some(match kind_of(status)? {
        ALREADY_EXISTS => routing::agent::Error::AlreadyExists(detail_of(status)?.to_string()),
        DISCONNECTED => routing::agent::Error::Disconnected,
        NULL_NEXT_HOP => routing::agent::Error::NullNextHop,
        VIA_OWN_NODE => routing::agent::Error::ViaOwnNode(detail_of(status)?.parse::<Eid>().ok()?),
        _ => return None,
    })
}

fn node_id_tag(e: &node_ids::Error) -> &'static str {
    match e {
        node_ids::Error::LocalNode => NODE_ID_LOCAL_NODE,
        node_ids::Error::NullEndpoint => NODE_ID_NULL_ENDPOINT,
        node_ids::Error::DtnWithDemux => NODE_ID_DTN_WITH_DEMUX,
        node_ids::Error::MultipleIpnNodeIds => NODE_ID_MULTIPLE_IPN,
        node_ids::Error::MultipleDtnNodeIds => NODE_ID_MULTIPLE_DTN,
        node_ids::Error::NoIpnNodeId => NODE_ID_NO_IPN,
        node_ids::Error::NoDtnNodeId => NODE_ID_NO_DTN,
        node_ids::Error::InvalidEid(_) => NODE_ID_INVALID_EID,
    }
}

// `invalid-eid` wraps a nested parse error that cannot travel, so it
// stays unrecoverable by design.
fn node_id_from_tag(tag: &str) -> Option<node_ids::Error> {
    Some(match tag {
        NODE_ID_LOCAL_NODE => node_ids::Error::LocalNode,
        NODE_ID_NULL_ENDPOINT => node_ids::Error::NullEndpoint,
        NODE_ID_DTN_WITH_DEMUX => node_ids::Error::DtnWithDemux,
        NODE_ID_MULTIPLE_IPN => node_ids::Error::MultipleIpnNodeIds,
        NODE_ID_MULTIPLE_DTN => node_ids::Error::MultipleDtnNodeIds,
        NODE_ID_NO_IPN => node_ids::Error::NoIpnNodeId,
        NODE_ID_NO_DTN => node_ids::Error::NoDtnNodeId,
        _ => return None,
    })
}
