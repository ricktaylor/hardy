/*!
The gRPC front door of a BPA: one bridge per component surface,
implementing the v1 wire contract against the public registration
traits of `hardy_bpa`. A host wires them up with one [`Signer`] and
its own `TaskPool`, one `<Surface>ServiceImpl` per enabled surface,
each wrapped in its generated `<Surface>ServiceServer`.

Each bridge follows the same design: `Subscribe` is the session (a
registration handshake, then a pure event stream), and every other
RPC presents the session token minted at registration. `session`
holds the shared session state (`Session`, `Sessions`), `token` the
bearer tokens; the session lifecycle is written concretely in each
surface under `services/`, one file per surface, with
`services/application.rs` as the template.
*/

mod adapter;
mod services;
mod session;
mod token;

pub use self::services::application::ApplicationServiceImpl;
pub use self::services::cla::ClaServiceImpl;
pub use self::services::routing::RoutingAgentServiceImpl;
pub use self::services::service::ServiceServiceImpl;
pub use self::token::Signer;

// The outbound buffer per session stream: events are small, so this
// only smooths bursts.
const CHANNEL_DEPTH: usize = 16;

// The outbound buffer for a data-plane transfer (delivery or forward),
// in [`CHUNK_SIZE`](crate::CHUNK_SIZE) slices. Deliberately shallow:
// HTTP/2 flow control does the real pacing, and anything staged here is
// bytes a client may abandon unread, so the resident cost per in-flight
// transfer is `DATA_CHANNEL_DEPTH * CHUNK_SIZE`. Tune against the
// negotiated flow-control window, not upward for its own sake.
const DATA_CHANNEL_DEPTH: usize = 4;
