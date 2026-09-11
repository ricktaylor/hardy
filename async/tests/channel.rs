#![cfg(feature = "tokio")]

use hardy_async::channel::{RecvError, SendError, TrySendError, bounded, unbounded};

#[tokio::test]
async fn unbounded_try_send_never_full() {
    let (tx, rx) = unbounded::<usize>();
    for i in 0..10_000 {
        assert_eq!(tx.try_send(i), Ok(()));
    }
    // Messages drain in FIFO order.
    for i in 0..10_000 {
        assert_eq!(rx.recv().await, Ok(i));
    }
}

#[tokio::test]
async fn unbounded_send_fails_after_receiver_dropped() {
    let (tx, rx) = unbounded::<i32>();
    drop(rx);
    assert_eq!(tx.send(7).await, Err(SendError(7)));
    assert_eq!(tx.try_send(8), Err(TrySendError::Disconnected(8)));
}

#[tokio::test]
async fn bounded_zero_is_rendezvous() {
    let (tx, rx) = bounded::<i32>(0);

    // No receiver is waiting, so a zero-capacity channel is always full.
    assert_eq!(tx.try_send(1), Err(TrySendError::Full(1)));

    // An awaited send parks until a receive is in progress.
    let send_fut = tx.send(2);
    futures::pin_mut!(send_fut);
    assert!(futures::poll!(send_fut.as_mut()).is_pending());

    let (sent, received) = futures::join!(send_fut, rx.recv());
    assert_eq!(sent, Ok(()));
    assert_eq!(received, Ok(2));
}

#[tokio::test]
async fn recv_disconnects_after_drain() {
    let (tx, rx) = bounded::<i32>(2);
    tx.try_send(1).unwrap();
    tx.try_send(2).unwrap();
    drop(tx);
    assert_eq!(rx.recv().await, Ok(1));
    assert_eq!(rx.recv().await, Ok(2));
    assert_eq!(rx.recv().await, Err(RecvError::Disconnected));
}

#[tokio::test]
async fn try_send_error_variants_return_the_message() {
    let (tx, rx) = bounded::<i32>(1);
    assert_eq!(tx.try_send(1), Ok(()));

    // Full with a live receiver; Disconnected once it is dropped. Both
    // variants hand the rejected message back.
    assert_eq!(tx.try_send(2), Err(TrySendError::Full(2)));
    drop(rx);
    assert_eq!(tx.try_send(3), Err(TrySendError::Disconnected(3)));
}
