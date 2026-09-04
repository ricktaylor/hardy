/*!
The client SDK: a local component registers against a remote BPA over
the v1 wire with the same traits a local [`Bpa`](hardy_bpa::bpa::Bpa)
uses, and the SDK carries the sessions, tokens, and data-plane calls.

All four surfaces are served: applications, low-level services,
convergence-layer adapters, and routing agents.
*/

mod adapter;
mod bpa_client;
mod collector;
mod services;

pub use bpa_client::{BpaClient, EndpointError, RegistrationHandle};

use core::num::NonZeroUsize;

// The request channel of one data-plane transfer (Send/Dispatch/Receive/
// Forward): the metadata message, then chunks written one at a time under
// backpressure. The capacity is load-bearing, not a tuning knob:
// [`adapter::Reader`]'s drop relies on there being room for every message
// this side queues plus the in-band cancel, so `try_send(cancel)` on drop
// is reliable rather than best-effort. Do not lower it below `queued + 1`.
pub(crate) const TRANSFER_REQUEST_CAPACITY: usize = 2;

// The request channel of a Subscribe session: the Register handshake plus
// a later Unregister, with headroom.
pub(crate) const SUBSCRIBE_REQUEST_CAPACITY: usize = 4;

// How many announced deliveries one registration collects at once, on the
// application and service surfaces alike: enough that one slow collection
// does not serialise the rest, small enough that a single registration
// cannot monopolise its connection. Beyond the bound, the announcement
// loop waits for a slot, which backpressures the session stream and
// through it the BPA, by design.
pub(crate) const MAX_CONCURRENT_DELIVERIES: NonZeroUsize = NonZeroUsize::new(4).unwrap();
