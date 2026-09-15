//! The per-session bounds a server places on its clients.

use core::{
    num::{NonZeroU64, NonZeroUsize},
    time::Duration,
};

use crate::{MAX_MESSAGE_SIZE, chunking::DEFAULT_CHUNK_SIZE, server::announce::DATA_CHANNEL_DEPTH};

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

/// The number of inbound transfers one session may have open at once: the
/// `Send` or `Dispatch` calls a client has opened and not yet finished.
///
/// A further call waits for one of them to end. Each open transfer holds a
/// message being decoded and a chunk being handed to the BPA, so this is what
/// bounds the memory a client can commit the server to by opening calls.
pub const MAX_INBOUND_TRANSFERS: usize = 4;

/// The buffers one stalled application or service session holds at most, in
/// bytes.
///
/// It is [`MAX_INBOUND_TRANSFERS`] transfers, each holding one inbound message
/// being decoded, at most [`MAX_MESSAGE_SIZE`], and one chunk in flight to the
/// BPA, at most [`DEFAULT_CHUNK_SIZE`]; plus one collection, whose channel
/// queues a fixed depth of chunks ahead of the client with the reserved
/// terminal slot beside them. A session that negotiated a smaller chunk size
/// holds less.
///
/// A CLA session is not bounded this way: the BPA forwards to a CLA once per
/// peer queue concurrently, so its collections scale with the peers it has.
pub const SESSION_FOOTPRINT: u64 = MAX_INBOUND_TRANSFERS as u64
    * (MAX_MESSAGE_SIZE as u64 + DEFAULT_CHUNK_SIZE as u64)
    + (DATA_CHANNEL_DEPTH as u64 + 1) * DEFAULT_CHUNK_SIZE as u64;

/// The bounds a server places, per API, on how long a client can make it wait.
///
/// The BPA offers deliveries and forwardings one at a time per endpoint or
/// peer, so a client that stops answering blocks a queue rather than one
/// bundle, and it costs the client nothing to do so. Each field bounds one way
/// of holding the server: a deadline on every wait a client can extend, a
/// minimum rate on every transfer, and a ceiling on live sessions. A client
/// that exceeds a bound has its session closed, its calls in flight ended with
/// `DEADLINE_EXCEEDED`, and any bundle it held mid-exchange kept by the BPA to
/// be offered again. The bounds sit above the transport on purpose: a client
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

    /// The time a transfer is allowed before [`min_rate`](Limits::min_rate) starts
    /// to apply.
    ///
    /// Has no effect when `min_rate` is `None`. Defaults to 30 seconds.
    pub grace: Duration,

    /// The rate, in bytes per second, a transfer must sustain once its grace is
    /// spent.
    ///
    /// A transfer's deadline is the earlier of [`idle`](Limits::idle) from now and,
    /// measured from the start of the transfer, [`grace`](Limits::grace) plus the
    /// time the bytes moved so far have earned at this rate. Time is earned only by
    /// moving bytes, so a client sending one byte at a time cannot keep a transfer
    /// open by resetting `idle`, and `idle` remains the ceiling on any single gap,
    /// so the rate rule can only tighten a bound. `None` disables the rule and
    /// leaves only `idle`. Defaults to 1024 bytes per second.
    pub min_rate: Option<NonZeroU64>,

    /// The number of sessions the API serves at once.
    ///
    /// A `Subscribe` that finds no free slot is refused with `RESOURCE_EXHAUSTED`
    /// before it registers anything, and a slot is held for the life of its
    /// session. An application or service session holds at most
    /// [`SESSION_FOOTPRINT`] bytes of buffers while stalled, so this ceiling times
    /// that footprint is the memory such an API can have committed to clients
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
    fn the_session_footprint_is_the_one_documented() {
        assert_eq!(
            SESSION_FOOTPRINT,
            4 * (16 + 1) * 1024 * 1024 + 5 * 1024 * 1024
        );
    }
}
