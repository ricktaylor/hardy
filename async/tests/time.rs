#![cfg(feature = "tokio")]

use hardy_async::time::sleep;

/// The paused clock only advances when an awaited timer forces it to, so
/// an unchanged clock proves the early return registered no timer.
#[tokio::test(start_paused = true)]
async fn sleep_zero_and_negative_return_without_sleeping() {
    let start = tokio::time::Instant::now();

    sleep(time::Duration::ZERO).await;
    sleep(time::Duration::seconds(-1)).await;

    assert_eq!(tokio::time::Instant::now(), start);
}

#[tokio::test(start_paused = true)]
async fn sleep_positive_sleeps_for_the_requested_duration() {
    let sleep_fut = sleep(time::Duration::seconds(5));
    futures::pin_mut!(sleep_fut);

    // Registers the timer; nothing has elapsed yet.
    assert!(futures::poll!(sleep_fut.as_mut()).is_pending());

    // Still pending short of the deadline: catches unit-conversion
    // mistakes that shorten the sleep.
    tokio::time::advance(std::time::Duration::from_secs(4)).await;
    assert!(futures::poll!(sleep_fut.as_mut()).is_pending());

    // Ready exactly at the deadline.
    tokio::time::advance(std::time::Duration::from_secs(1)).await;
    assert!(futures::poll!(sleep_fut.as_mut()).is_ready());
}
