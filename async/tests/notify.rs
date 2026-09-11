#![cfg(feature = "tokio")]

use core::time::Duration;

use hardy_async::Notify;

/// A notification sent before any task waits is stored, so the next
/// `notified()` completes immediately.
#[tokio::test(start_paused = true)]
async fn notify_one_before_waiter_stores_permit() {
    let notify = Notify::new();
    notify.notify_one();

    assert!(
        tokio::time::timeout(Duration::from_secs(1), notify.notified())
            .await
            .is_ok()
    );
}

/// `notify_waiters` does not store a permit: with nobody waiting the
/// notification is lost, and a later `notified()` never completes. The
/// paused clock auto-advances, so the elapsed timeout is deterministic.
#[tokio::test(start_paused = true)]
async fn notify_waiters_before_waiter_is_lost() {
    let notify = Notify::new();
    notify.notify_waiters();

    assert!(
        tokio::time::timeout(Duration::from_secs(1), notify.notified())
            .await
            .is_err()
    );
}

/// `notify_waiters` wakes a waiter that is already registered (first
/// polled before the call).
#[tokio::test]
async fn notify_waiters_wakes_registered_waiter() {
    let notify = Notify::new();

    let notified = notify.notified();
    futures::pin_mut!(notified);
    assert!(futures::poll!(notified.as_mut()).is_pending());

    notify.notify_waiters();
    assert!(futures::poll!(notified.as_mut()).is_ready());
}
