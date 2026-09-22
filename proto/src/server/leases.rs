// Deadlines for client-driven waits, and a handle on session
// teardown.

use core::{
    fmt::{self, Display, Formatter},
    time::Duration,
};

use hardy_async::{CancellationToken, DropGuard};
use tokio::time::sleep;
use tracing::warn;

use crate::server::error::Error;

const DEFAULT_LEASE: Duration = Duration::from_secs(30);

// A kind of client wait the session bounds with a deadline.
#[derive(Debug, Clone, Copy)]
pub enum Lease {
    Claim,
    Feed,
    Drain,
    Ack,
    Event,
}

impl Display for Lease {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Claim => "claim",
            Self::Feed => "feed",
            Self::Drain => "drain",
            Self::Ack => "ack",
            Self::Event => "event",
        })
    }
}

/// Deadlines a surface holds each client session to.
///
/// Letting any deadline expire closes the session. Each defaults to 30
/// seconds.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    /// How long an announced bundle waits for its collecting call.
    pub claim: Duration,

    /// How long a transfer waits for the client's next chunk.
    pub feed: Duration,

    /// How long one chunk waits for room on the response side.
    pub drain: Duration,

    /// How long a completed delivery waits for the client's `Ack`.
    pub ack: Duration,

    /// How long an event waits for room on a full Subscribe stream.
    pub event: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            claim: DEFAULT_LEASE,
            feed: DEFAULT_LEASE,
            drain: DEFAULT_LEASE,
            ack: DEFAULT_LEASE,
            event: DEFAULT_LEASE,
        }
    }
}

impl Limits {
    pub(crate) async fn expire(&self, lease: Lease) {
        sleep(match lease {
            Lease::Claim => self.claim,
            Lease::Feed => self.feed,
            Lease::Drain => self.drain,
            Lease::Ack => self.ack,
            Lease::Event => self.event,
        })
        .await;
    }
}

// A handle on one session's teardown and deadlines.
#[derive(Clone)]
pub struct Leases {
    cancel: CancellationToken,
    limits: Limits,
    // Names the surface in warnings; the session token must never
    // reach a log.
    label: &'static str,
}

impl Leases {
    pub fn new(cancel: CancellationToken, label: &'static str, limits: Limits) -> Self {
        Self {
            cancel,
            limits,
            label,
        }
    }

    pub fn cancelled(&self) -> impl Future<Output = ()> + Send + '_ {
        self.cancel.cancelled()
    }

    // Returns a guard that aborts the session when dropped.
    pub fn guard(&self) -> DropGuard {
        self.cancel.clone().drop_guard()
    }

    pub(super) fn abort(&self) {
        self.cancel.cancel();
    }

    // Waits for `lease` to expire, then closes the session and returns
    // `Error::LeaseExpired`.
    pub async fn expired(&self, lease: Lease) -> Error {
        self.limits.expire(lease).await;

        if !self.cancel.is_cancelled() {
            warn!(
                "Closing a {} session: its client let the {lease} lease expire",
                self.label
            );
            self.abort();
        }
        Error::LeaseExpired(lease)
    }
}
