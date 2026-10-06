//! The per-session bounds a server places on its clients.

use core::{
    num::{NonZeroU64, NonZeroUsize},
    time::Duration,
};

use crate::{
    MAX_MESSAGE_SIZE,
    chunking::{BUFFERED_CHUNKS, DEFAULT_CHUNK_SIZE, MAX_CHUNK_SIZE},
};

/// The default [`Limits::handshake`]: short, because a call that has not
/// identified its session yet is worth nothing to anyone.
const DEFAULT_HANDSHAKE: Duration = Duration::from_secs(10);

/// The default [`Limits::idle`].
const DEFAULT_IDLE: Duration = Duration::from_secs(30);

/// The default [`Limits::claim`].
const DEFAULT_CLAIM: Duration = Duration::from_secs(30);

/// The default [`Limits::grace`].
const DEFAULT_GRACE: Duration = Duration::from_secs(30);

/// The default [`Limits::min_rate`].
const DEFAULT_MIN_RATE: Option<NonZeroU64> = NonZeroU64::new(1024);

/// The default [`Limits::max_sessions`].
const DEFAULT_MAX_SESSIONS: NonZeroUsize = NonZeroUsize::new(64).unwrap();

/// The largest encoded gRPC message, in bytes, a server sends or accepts.
///
/// A session this server runs never carries a chunk above
/// [`DEFAULT_CHUNK_SIZE`], so nothing either end sends needs more than one of
/// those plus the room [`MAX_CHUNK_SIZE`] was chosen to leave for the rest of
/// a message. It is well below the SDK's [`MAX_MESSAGE_SIZE`], which must also
/// fit a chunk from a server that negotiates higher, and it is what bounds
/// what a client can make this server allocate per call.
pub const MAX_SERVER_MESSAGE_SIZE: usize = DEFAULT_CHUNK_SIZE + (MAX_MESSAGE_SIZE - MAX_CHUNK_SIZE);

/// The number of inbound transfers one session may have open at once: the
/// `Send` or `Dispatch` calls a client has opened and not yet finished.
///
/// A further call waits for one of them to end, for at most
/// [`Limits::idle`], and is then refused with `RESOURCE_EXHAUSTED` on its own,
/// without closing the session. Each open transfer holds a message being
/// decoded and a chunk being handed to the BPA, so this is what bounds the
/// memory a client can commit the server to with transfers that are moving. A
/// call parked on this bound holds only its metadata, its HTTP/2 stream and
/// what the client has pushed into that stream's flow-control window, and how
/// many may park at once is the transport's `max_concurrent_streams`, not a
/// bound of this crate.
pub const MAX_INBOUND_TRANSFERS: usize = 4;

/// The number of forwardings one convergence layer session may have in flight
/// at once.
///
/// The BPA forwards to a CLA once per peer queue concurrently, and a client
/// adds peers itself, so without this bound the buffers one session can hold
/// grow with the peers it has asked for. A forwarding beyond it waits its
/// turn, which backpressures the queue it came from rather than failing it.
/// Each one in flight holds the chunks queued ahead of the client.
pub const MAX_OUTBOUND_FORWARDINGS: usize = 4;

/// The chunk buffers one stalled application or service session holds at most,
/// in bytes.
///
/// It is [`MAX_INBOUND_TRANSFERS`] transfers, each holding one inbound message
/// being decoded, at most [`MAX_SERVER_MESSAGE_SIZE`], and one chunk in flight to the
/// BPA, at most [`DEFAULT_CHUNK_SIZE`]; plus one collection, whose channel
/// queues a fixed depth of chunks ahead of the client with the reserved
/// terminal slot beside them. A session that negotiated a smaller chunk size
/// holds less. It counts chunk buffers alone: the event channel, which holds
/// small messages, and the calls parked on [`MAX_INBOUND_TRANSFERS`], which
/// the transport bounds, sit outside it.
///
/// A CLA session holds more, because the BPA forwards to it once per peer
/// queue concurrently: see [`CLA_SESSION_FOOTPRINT`].
pub const SESSION_FOOTPRINT: u64 = MAX_INBOUND_TRANSFERS as u64
    * (MAX_SERVER_MESSAGE_SIZE as u64 + DEFAULT_CHUNK_SIZE as u64)
    + (BUFFERED_CHUNKS as u64 + 1) * DEFAULT_CHUNK_SIZE as u64;

/// The chunk buffers one stalled convergence layer session holds at most, in
/// bytes.
///
/// It is the inbound side of [`SESSION_FOOTPRINT`], which a CLA session
/// bounds the same way, plus [`MAX_OUTBOUND_FORWARDINGS`] collections rather
/// than one, since the BPA forwards once per peer queue concurrently. A
/// session that negotiated a smaller chunk size holds less.
pub const CLA_SESSION_FOOTPRINT: u64 = MAX_INBOUND_TRANSFERS as u64
    * (MAX_SERVER_MESSAGE_SIZE as u64 + DEFAULT_CHUNK_SIZE as u64)
    + MAX_OUTBOUND_FORWARDINGS as u64 * (BUFFERED_CHUNKS as u64 + 1) * DEFAULT_CHUNK_SIZE as u64;

/// The bounds a server places, per API, on how long a client can make it wait.
///
/// The BPA offers deliveries and forwardings one at a time per endpoint or
/// peer, so a client that stops answering blocks a queue rather than one
/// bundle, and it costs the client nothing to do so. Each field bounds one way
/// of holding the server: a deadline on every wait a client can extend, a
/// minimum rate on every transfer, and a ceiling on live sessions. A client
/// that exceeds a bound has its session closed: the wait that missed its bound
/// ends with `DEADLINE_EXCEEDED`, every other call in flight on the session
/// ends with `UNAVAILABLE`, and any bundle it held mid-exchange is kept by the
/// BPA to be offered again. The bounds sit above the transport on purpose: a client
/// that answers HTTP/2 keep-alive pings while refusing to read or write is
/// invisible to keep-alive.
///
/// `Default` yields the value documented on each field. Pass a `Limits` to the
/// `with_limits` constructor of an API. A zero duration means the bound has
/// already passed, so the first wait it covers stalls at once; it is useful in
/// tests and is a misconfiguration anywhere else.
///
/// # Examples
///
/// ```
/// use core::time::Duration;
///
/// use hardy_proto::server::Limits;
///
/// let limits = Limits {
///     handshake: Duration::from_secs(5),
///     ..Limits::default()
/// };
/// assert_eq!(limits.idle, Duration::from_secs(30));
/// assert_eq!(limits.max_sessions.get(), 64);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// The longest a call may take to send its first message: the `Register` that
    /// opens a `Subscribe`, or the metadata message that opens a streaming
    /// data-plane call.
    ///
    /// A call that exceeds it fails with `DEADLINE_EXCEEDED`. Nothing else is
    /// affected, because no session has been named yet. Defaults to 10 seconds.
    pub handshake: Duration,

    /// The longest a client may leave any single wait in a live session unanswered.
    ///
    /// The waits are for the next inbound chunk of a transfer, for room to send the
    /// next outbound chunk, for the `ack` or result once the client has taken the
    /// last chunk, and for room on the session stream for the next event.
    /// Exceeding it closes the session. Defaults to 30 seconds.
    pub idle: Duration,

    /// The longest an announced delivery or forwarding may wait for the call that
    /// collects it.
    ///
    /// Exceeding it closes the session, and the BPA keeps the bundle. Defaults to
    /// 30 seconds.
    pub claim: Duration,

    /// The time a transfer may spend waiting for its client before
    /// [`min_rate`](Limits::min_rate) starts to apply.
    ///
    /// Has no effect when `min_rate` is `None`. Defaults to 30 seconds.
    pub grace: Duration,

    /// The rate, in bytes per second, a transfer must sustain once its grace is
    /// spent.
    ///
    /// A transfer begins with [`grace`](Limits::grace) and earns more at this rate
    /// as it moves bytes. Only the time it spends waiting for the client is spent
    /// out of that, so the server's own work between one chunk and the next is
    /// never charged to the client. Time is earned only by moving bytes, so a
    /// client moving one byte at a time cannot hold a transfer open by resetting
    /// `idle`, and [`idle`](Limits::idle) remains the ceiling on any single wait,
    /// so the rate rule can only tighten a bound. `None` disables the rule and
    /// leaves only `idle`. Defaults to 1024 bytes per second.
    pub min_rate: Option<NonZeroU64>,

    /// The number of sessions the API serves at once.
    ///
    /// A `Subscribe` that finds no free slot is refused with `RESOURCE_EXHAUSTED`
    /// before it registers anything, and a slot is held for the life of its
    /// session. A stalled session holds at most [`SESSION_FOOTPRINT`] bytes of
    /// buffers, or [`CLA_SESSION_FOOTPRINT`] on the CLA API, so this ceiling
    /// times that footprint is the memory an API can have committed to clients
    /// that have stopped answering. Defaults to 64 sessions.
    pub max_sessions: NonZeroUsize,
}

/// Yields the value documented on each field.
impl Default for Limits {
    fn default() -> Self {
        Self {
            handshake: DEFAULT_HANDSHAKE,
            idle: DEFAULT_IDLE,
            claim: DEFAULT_CLAIM,
            grace: DEFAULT_GRACE,
            min_rate: DEFAULT_MIN_RATE,
            max_sessions: DEFAULT_MAX_SESSIONS,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_defaults_are_the_ones_documented() {
        let limits = Limits::default();

        assert_eq!(limits.handshake, Duration::from_secs(10));
        assert_eq!(limits.idle, Duration::from_secs(30));
        assert_eq!(limits.claim, Duration::from_secs(30));
        assert_eq!(limits.grace, Duration::from_secs(30));
        assert_eq!(limits.min_rate.map(NonZeroU64::get), Some(1024));
        assert_eq!(limits.max_sessions.get(), 64);
    }

    #[test]
    fn the_server_only_decodes_what_a_session_can_carry() {
        assert_eq!(MAX_SERVER_MESSAGE_SIZE, 2 * 1024 * 1024);
        const {
            assert!(
                MAX_SERVER_MESSAGE_SIZE > DEFAULT_CHUNK_SIZE,
                "a chunk and the rest of its message must fit in one message"
            )
        };
    }

    #[test]
    fn the_session_footprint_is_the_one_documented() {
        assert_eq!(
            SESSION_FOOTPRINT,
            4 * (2 + 1) * 1024 * 1024 + 5 * 1024 * 1024
        );
        assert_eq!(
            CLA_SESSION_FOOTPRINT,
            4 * (2 + 1) * 1024 * 1024 + 4 * 5 * 1024 * 1024
        );
    }
}
