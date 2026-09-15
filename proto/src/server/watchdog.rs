//! The per-session bounds on how long a client can make the server wait.

use core::fmt::{self, Display, Formatter};

use tokio::{
    sync::watch::{Sender, channel},
    time::{Instant, sleep_until},
};
use tonic::Status;
use tracing::warn;

use crate::server::limits::Limits;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    /// Waiting for the call that collects an announced delivery or forwarding.
    Claim,
    /// Waiting for the client's next inbound chunk.
    Feed,
    /// Waiting for the client to make room for the next outbound chunk.
    Drain,
    /// Waiting for the client's `ack` or result after the last chunk.
    Ack,
    /// Waiting for the client to make room on the session stream for the next
    /// event.
    Event,
}

/// Formats as the lowercase stage name, as it appears in the log.
impl Display for Stage {
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

/// A session's stall detector.
///
/// Each wait a client can extend races against [`idle`](Watchdog::idle) or
/// [`claim`](Watchdog::claim), which resolve with a `DEADLINE_EXCEEDED` status
/// once the corresponding [`Limits`] bound passes, or is bounded by the transfer
/// it belongs to, whose owner then [`record`](Watchdog::record)s the stall. The
/// first stall is recorded and [`stalled`](Watchdog::stalled) wakes the session
/// task, so the session closes on its first stall; later stalls do not
/// overwrite the record.
pub struct Watchdog {
    limits: Limits,
    stall: Sender<Option<Stage>>,
}

impl Watchdog {
    /// Creates a watchdog enforcing `limits`. Nothing is armed until a wait calls
    /// one of the bounding methods.
    pub fn new(limits: Limits) -> Self {
        Self {
            limits,
            stall: channel(None).0,
        }
    }

    /// Resolves with the stage of the first stall, once there has been one.
    ///
    /// A watchdog that never fires never resolves; a call made after the stall
    /// resolves at once.
    pub async fn stalled(&self) -> Stage {
        let mut stall = self.stall.subscribe();
        stall
            .wait_for(Option::is_some)
            .await
            .ok()
            .and_then(|stage| *stage)
            .expect("the watchdog owns the stall sender for its whole life")
    }

    /// Resolves as a stall at `stage` once [`Limits::idle`] has elapsed.
    pub async fn idle(&self, stage: Stage) -> Status {
        self.expire_at(Instant::now() + self.limits.idle, stage)
            .await
    }

    /// Resolves as a stall at [`Stage::Claim`] once [`Limits::claim`] has elapsed.
    pub async fn claim(&self) -> Status {
        self.expire_at(Instant::now() + self.limits.claim, Stage::Claim)
            .await
    }

    /// Returns the limits the watchdog enforces.
    pub fn limits(&self) -> Limits {
        self.limits
    }

    /// Records a stall at `stage` and returns the status the stalled wait ends
    /// with.
    ///
    /// A transfer bounded on its own, by a
    /// [`BoundedChunkReceiver`](crate::chunking::BoundedChunkReceiver) or
    /// [`BoundedChunkSender`](crate::chunking::BoundedChunkSender), reports a
    /// [`TransferError::Stalled`](crate::chunking::TransferError::Stalled) to the call that
    /// owns it, which names the stage here.
    pub fn stall(&self, stage: Stage) -> Status {
        self.record(stage);
        // The stage names the server's internals, so it is logged rather than
        // returned: a client can do nothing with it that it cannot do with the
        // fact that it was too slow.
        Status::deadline_exceeded("timed out waiting for the client")
    }

    /// Records a stall at `stage`, unless one is recorded already, and wakes
    /// [`stalled`](Watchdog::stalled).
    fn record(&self, stage: Stage) {
        let first = self.stall.send_if_modified(|stall| match stall {
            Some(_) => false,
            None => {
                *stall = Some(stage);
                true
            }
        });
        if first {
            warn!("slow consumer detected while waiting for {stage}");
        }
    }

    /// Sleeps until `deadline`, then records `stage` as the stall.
    async fn expire_at(&self, deadline: Instant, stage: Stage) -> Status {
        sleep_until(deadline).await;
        self.stall(stage)
    }
}

#[cfg(test)]
mod tests {
    use core::time::Duration;
    use std::sync::Arc;

    use tokio::time::sleep;
    use tonic::Code;

    use super::*;

    /// Asserts that `status` is what an outlasted bound tells the client, and
    /// that it names no stage.
    fn assert_slow_consumer(status: &Status) {
        assert_eq!(status.code(), Code::DeadlineExceeded);
        assert_eq!(status.message(), "timed out waiting for the client");
    }

    #[tokio::test(start_paused = true)]
    async fn the_first_stall_is_the_one_recorded() {
        let watchdog = Watchdog::new(Limits::default());

        assert_slow_consumer(&watchdog.idle(Stage::Ack).await);
        assert_eq!(watchdog.stalled().await, Stage::Ack);

        assert_slow_consumer(&watchdog.claim().await);
        assert_eq!(
            watchdog.stalled().await,
            Stage::Ack,
            "a later stall must not overwrite the recorded cause"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_stall_wakes_a_waiting_session() {
        let watchdog = Arc::new(Watchdog::new(Limits::default()));
        let door = tokio::spawn({
            let watchdog = watchdog.clone();
            async move { watchdog.idle(Stage::Ack).await }
        });

        assert_eq!(watchdog.stalled().await, Stage::Ack);
        assert_slow_consumer(&door.await.expect("the door task must not panic"));
    }

    #[tokio::test(start_paused = true)]
    async fn a_recorded_stall_wakes_a_late_waiter() {
        let watchdog = Watchdog::new(Limits::default());

        watchdog.idle(Stage::Event).await;

        assert_eq!(watchdog.stalled().await, Stage::Event);
    }

    #[tokio::test(start_paused = true)]
    async fn a_quiet_watchdog_never_wakes_a_waiter() {
        let watchdog = Watchdog::new(Limits::default());

        let woken = tokio::select! {
            biased;
            _ = watchdog.stalled() => true,
            () = sleep(Duration::from_secs(3600)) => false,
        };

        assert!(
            !woken,
            "a watchdog that never fired must not report a stall"
        );
    }
}
