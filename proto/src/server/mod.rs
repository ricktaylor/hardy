/*!
The gRPC front door of a BPA: one service per component surface,
implementing the v1 wire contract against the public registration
traits of `hardy_bpa`. A host wires them up with its own `TaskPool`,
one `<Surface>ServiceImpl` per enabled surface, each wrapped in its
generated `<Surface>ServiceServer` and sized to the wire's message
caps ([`MAX_MESSAGE_SIZE`](crate::MAX_MESSAGE_SIZE) in both
directions, matching the client SDK's ends).

Every surface follows the same design: `Subscribe` is the session (a
registration, then a pure event stream), and every other RPC presents
the session token minted at registration. `subscribe` serves that rpc
for all four, from registration to unregistration, against each
surface's own `SubscribeHandler`, and holds the live-session map a
data-plane door resolves its token against; `session` holds the
per-session state (`Session`, its stream and guard); `slot` holds what
a registration hands its surface; `announce` holds the
announce-and-collect table; and the surfaces themselves live under
`services/`, one file per surface, with `services/application.rs` as
the template.
*/

mod adapter;
mod announce;
mod services;
mod session;
mod slot;
mod subscribe;

pub use self::services::{
    application::ApplicationServiceImpl, cla::ClaServiceImpl, routing::RoutingAgentServiceImpl,
    service::ServiceServiceImpl,
};

/// Why an exchange with the component behind a session did not happen.
/// Every variant leaves the work with the BPA; they differ in what they
/// say about the session, which is what a surface reports upwards.
#[derive(Debug, thiserror::Error)]
pub(crate) enum SessionError {
    /// The session was torn down: before it could take an event, or
    /// before any door collected an announcement.
    #[error("The session is closed")]
    Closed,

    /// A later announcement of the same bundle id took this one's
    /// place, and the door will collect that one. The session is alive,
    /// so this is one exchange failing, not a disconnection.
    #[error("A later announcement superseded this one")]
    Superseded,
}

// The outbound buffer per session stream: events are small, so this
// only smooths bursts.
const CHANNEL_DEPTH: usize = 16;

// The outbound buffer for a data-plane transfer, in
// [`CHUNK_SIZE`](crate::CHUNK_SIZE) slices. Shallow on purpose: HTTP/2
// flow control does the real pacing, and the resident cost per
// in-flight transfer is `DATA_CHANNEL_DEPTH * CHUNK_SIZE`. Tune against
// the negotiated flow-control window.
const DATA_CHANNEL_DEPTH: usize = 4;
