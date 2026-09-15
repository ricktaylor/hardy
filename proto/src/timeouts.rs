//! The per-session bounds on how long a client can make the server wait.

use core::{
    fmt::{self, Display, Formatter},
    future::Future,
};

use hardy_async::CancellationToken;
use tokio::{
    sync::watch::{Sender, channel},
    time::sleep,
};
use tonic::Status;
use tracing::warn;

use crate::limits::Limits;

/// The message every wait a client outlasted ends with. The stage is logged
/// and never sent: a client can do nothing with the server's internals that it
/// cannot do with the fact that it was too slow.
pub const SLOW_CONSUMER: &str = "timed out waiting for the client";

/// The waits a client can stall the server in, each held to one [`Limits`]
/// bound.
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

/// The deadlines a session holds its client to, and the first one it missed.
///
/// Each wait a client can extend is either run under
/// [`bound`](Timeouts::bound), which ends it with `DEADLINE_EXCEEDED` once the
/// [`Limits`] bound of its stage passes, or bounded by the transfer it belongs
/// to, whose owner then calls [`time_out`](Timeouts::time_out). The
/// first timeout is the one recorded, and it wakes
/// [`timed_out`](Timeouts::timed_out) and so the session task, so a session
/// closes on the first bound its client missed and a later one does not
/// overwrite the record.
pub struct Timeouts {
    limits: Limits,
    cancel: CancellationToken,
    first: Sender<Option<Stage>>,
}

impl Timeouts {
    /// Creates the timeouts of a session that `cancel` ends and `limits`
    /// bounds. Nothing is armed until a wait calls one of the bounding
    /// methods.
    pub fn new(limits: Limits, cancel: CancellationToken) -> Self {
        Self {
            limits,
            cancel,
            first: channel(None).0,
        }
    }

    /// The token that ends the session these timeouts belong to.
    pub fn cancel_token(&self) -> &CancellationToken {
        &self.cancel
    }

    /// Resolves once the session has ended, whatever ended it.
    pub async fn cancelled(&self) {
        self.cancel.cancelled().await;
    }

    /// Resolves with the stage of the first timeout, once there has been one.
    ///
    /// A session that never misses a bound never resolves; a call made after
    /// the timeout resolves at once.
    pub async fn timed_out(&self) -> Stage {
        let mut first = self.first.subscribe();
        first
            .wait_for(Option::is_some)
            .await
            .ok()
            .and_then(|stage| *stage)
            .expect("a session owns its timeouts, and so this sender, for its whole life")
    }

    /// Runs `work` under the bound `stage` is held to.
    ///
    /// The session's ending wins over the work, and the work over the bound,
    /// so a session that is already going does not report a timeout and a
    /// client that answers in time is never charged for the race.
    ///
    /// # Errors
    ///
    /// Returns `UNAVAILABLE` if the session ends first, `DEADLINE_EXCEEDED` if
    /// the bound passes first, which is recorded at `stage`, and whatever
    /// `work` itself fails with.
    pub async fn bound<T>(
        &self,
        stage: Stage,
        work: impl Future<Output = Result<T, Status>>,
    ) -> Result<T, Status> {
        tokio::select! {
            biased;
            () = self.cancelled() => Err(Status::unavailable("registration closed")),
            result = work => result,
            timed_out = self.expire(stage) => Err(timed_out),
        }
    }

    /// Resolves as a timeout once the bound of `stage` has elapsed.
    ///
    /// [`Stage::Claim`] is held to [`Limits::claim`], every other stage to
    /// [`Limits::idle`], so the stage a wait names is the bound it gets.
    async fn expire(&self, stage: Stage) -> Status {
        let bound = match stage {
            Stage::Claim => self.limits.claim,
            Stage::Feed | Stage::Drain | Stage::Ack | Stage::Event => self.limits.idle,
        };
        // `sleep` saturates where `Instant + Duration` would overflow, so a
        // host that sets a bound to `Duration::MAX` has disabled it.
        sleep(bound).await;
        self.time_out(stage)
    }

    /// Returns the limits these timeouts are read from.
    pub fn limits(&self) -> Limits {
        self.limits
    }

    /// Records a timeout at `stage` and returns the status the wait that
    /// missed its bound ends with.
    ///
    /// A transfer bounded on its own, by a
    /// [`BoundedChunkReceiver`](crate::chunking::BoundedChunkReceiver) or
    /// [`BoundedChunkSender`](crate::chunking::BoundedChunkSender), reports a
    /// [`TransferError::TimedOut`](crate::chunking::TransferError::TimedOut) to the call that
    /// owns it, which names the stage here.
    pub fn time_out(&self, stage: Stage) -> Status {
        self.record(stage);
        Status::deadline_exceeded(SLOW_CONSUMER)
    }

    /// Records a timeout at `stage`, unless one is recorded already, and wakes
    /// [`timed_out`](Timeouts::timed_out).
    fn record(&self, stage: Stage) {
        let first = self.first.send_if_modified(|first| match first {
            Some(_) => false,
            None => {
                *first = Some(stage);
                true
            }
        });
        if first {
            warn!("slow consumer detected while waiting for {stage}");
        }
    }
}

#[cfg(test)]
mod tests {
    use core::{
        future::pending,
        pin::pin,
        task::{Context, Waker},
    };
    use std::sync::Arc;

    use tonic::Code;

    use super::*;

    // Asserts that `status` is what an outlasted bound tells the client, and
    // that it names no stage.
    fn assert_slow_consumer(status: &Status) {
        assert_eq!(status.code(), Code::DeadlineExceeded);
        assert_eq!(status.message(), SLOW_CONSUMER);
    }

    #[tokio::test(start_paused = true)]
    async fn the_first_timeout_is_the_one_recorded() {
        let timeouts = Timeouts::new(Limits::default(), CancellationToken::new());

        assert_slow_consumer(&timeouts.expire(Stage::Ack).await);
        assert_eq!(timeouts.timed_out().await, Stage::Ack);

        assert_slow_consumer(&timeouts.expire(Stage::Claim).await);
        assert_eq!(
            timeouts.timed_out().await,
            Stage::Ack,
            "a later timeout must not overwrite the recorded cause"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn work_that_answers_within_the_bound_is_not_charged() {
        let timeouts = Timeouts::new(Limits::default(), CancellationToken::new());

        let answered = timeouts
            .bound(Stage::Ack, async { Ok(7) })
            .await
            .expect("work that answers at once must not be bounded out");

        assert_eq!(answered, 7);
        assert!(
            timeouts.first.borrow().is_none(),
            "work that answered must leave no timeout recorded"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_session_that_has_ended_reports_no_timeout() {
        let cancel = CancellationToken::new();
        let timeouts = Timeouts::new(Limits::default(), cancel.clone());
        cancel.cancel();

        let status = timeouts
            .bound(Stage::Ack, pending::<Result<(), Status>>())
            .await
            .expect_err("a session that has ended must end its waits");

        assert_eq!(status.code(), Code::Unavailable);
        assert_eq!(status.message(), "registration closed");
        assert!(
            timeouts.first.borrow().is_none(),
            "a session that ended on its own must not be charged a timeout"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_timeout_wakes_a_waiting_session() {
        let timeouts = Arc::new(Timeouts::new(Limits::default(), CancellationToken::new()));
        let door = tokio::spawn({
            let timeouts = timeouts.clone();
            async move { timeouts.expire(Stage::Ack).await }
        });

        assert_eq!(timeouts.timed_out().await, Stage::Ack);
        assert_slow_consumer(&door.await.expect("the door task must not panic"));
    }

    #[tokio::test(start_paused = true)]
    async fn a_recorded_timeout_wakes_a_late_waiter() {
        let timeouts = Timeouts::new(Limits::default(), CancellationToken::new());

        timeouts.expire(Stage::Event).await;

        assert_eq!(timeouts.timed_out().await, Stage::Event);
    }

    #[tokio::test(start_paused = true)]
    async fn a_session_that_misses_no_bound_wakes_no_waiter() {
        let timeouts = Timeouts::new(Limits::default(), CancellationToken::new());
        timeouts
            .bound(Stage::Ack, async { Ok(()) })
            .await
            .expect("work that answers at once must not be bounded out");

        let mut waiter = pin!(timeouts.timed_out());
        assert!(
            waiter
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_pending(),
            "a session that has missed no bound must report no timeout"
        );
    }
}
