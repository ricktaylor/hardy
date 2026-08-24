#![cfg(feature = "tokio")]

use hardy_async::closeable::{RecvError, SendError, TrySendError, bounded};

#[tokio::test]
async fn send_recv_round_trip() {
    let (tx, rx) = bounded::<i32>(4);
    assert!(tx.send(1).await.is_ok());
    assert!(tx.send(2).await.is_ok());
    assert_eq!(rx.recv().await, Ok(1));
    assert_eq!(rx.recv().await, Ok(2));
}

#[tokio::test]
async fn close_rejects_subsequent_send() {
    let (tx, _rx) = bounded::<i32>(4);
    let tx2 = tx.clone();
    tx.close();
    assert!(matches!(tx2.send(1).await, Err(SendError(1))));
}

#[tokio::test]
async fn close_rejects_subsequent_try_send() {
    let (tx, _rx) = bounded::<i32>(4);
    let tx2 = tx.clone();
    tx.close();
    assert!(matches!(
        tx2.try_send(1),
        Err(TrySendError::Disconnected(1))
    ));
}

#[tokio::test]
async fn buffered_messages_drain_then_disconnect() {
    let (tx, rx) = bounded::<i32>(4);
    let tx2 = tx.clone();
    assert!(tx2.send(1).await.is_ok());
    assert!(tx2.send(2).await.is_ok());
    assert!(tx2.send(3).await.is_ok());
    tx.close();
    assert_eq!(rx.recv().await, Ok(1));
    assert_eq!(rx.recv().await, Ok(2));
    assert_eq!(rx.recv().await, Ok(3));
    assert_eq!(rx.recv().await, Err(RecvError::Disconnected));
}

#[tokio::test]
async fn close_visible_to_clones() {
    let (tx, _rx) = bounded::<i32>(4);
    let tx2 = tx.clone();
    let tx3 = tx.clone();
    tx.close();
    assert!(tx2.send(1).await.is_err());
    assert!(tx3.try_send(1).is_err());
}

#[tokio::test]
async fn close_is_idempotent_across_clones() {
    let (tx, rx) = bounded::<i32>(4);
    let tx2 = tx.clone();
    let tx3 = tx.clone();

    tx.send(1).await.unwrap();
    tx.close();

    // Repeated close on other clones is a no-op: it neither re-opens the
    // channel nor disturbs the buffered message.
    tx2.close();
    tx3.close();

    assert!(matches!(
        tx2.try_send(2),
        Err(TrySendError::Disconnected(2))
    ));
    assert!(matches!(tx3.send(3).await, Err(SendError(3))));

    // The message buffered before the first close is delivered exactly
    // once, then the channel reports disconnection on every recv.
    assert_eq!(rx.recv().await, Ok(1));
    assert_eq!(rx.recv().await, Err(RecvError::Disconnected));
    assert_eq!(rx.recv().await, Err(RecvError::Disconnected));
}

#[tokio::test]
async fn try_send_full_distinct_from_disconnected() {
    let (tx, _rx) = bounded::<i32>(2);
    assert!(tx.try_send(1).is_ok());
    assert!(tx.try_send(2).is_ok());
    assert!(matches!(tx.try_send(3), Err(TrySendError::Full(3))));
}

#[tokio::test]
async fn dropping_receiver_disconnects_sender() {
    let (tx, rx) = bounded::<i32>(4);
    drop(rx);
    assert!(tx.send(1).await.is_err());
    assert!(matches!(tx.try_send(2), Err(TrySendError::Disconnected(2))));
}

#[tokio::test]
async fn recv_disconnects_on_close_with_empty_buffer() {
    let (tx, rx) = bounded::<i32>(4);
    tx.close();
    assert_eq!(rx.recv().await, Err(RecvError::Disconnected));
}

#[tokio::test]
async fn recv_disconnects_when_all_senders_dropped() {
    let (tx, rx) = bounded::<i32>(4);
    drop(tx);
    assert_eq!(rx.recv().await, Err(RecvError::Disconnected));
}

/// Pins the documented `close` semantics: a `send` already parked on a
/// full buffer is not interrupted by `close`, and completes once the
/// receiver frees space.
#[tokio::test]
async fn parked_send_survives_close() {
    let (tx, rx) = bounded::<i32>(1);
    tx.send(1).await.unwrap();

    // Parks on the full buffer.
    let send_fut = tx.send(2);
    futures::pin_mut!(send_fut);
    assert!(futures::poll!(send_fut.as_mut()).is_pending());

    // close() does not abort the parked send.
    tx.close();
    assert!(futures::poll!(send_fut.as_mut()).is_pending());

    // Receiving frees space; the parked send now lands its message.
    assert_eq!(rx.recv().await, Ok(1));
    assert_eq!(
        futures::poll!(send_fut.as_mut()),
        core::task::Poll::Ready(Ok(()))
    );

    // The late message is buffered, so a receiver that keeps polling
    // still observes it before disconnection.
    assert_eq!(rx.recv().await, Ok(2));
    assert_eq!(rx.recv().await, Err(RecvError::Disconnected));
}

#[tokio::test]
async fn len_reports_buffer_fill() {
    let (tx, rx) = bounded::<i32>(4);
    assert_eq!(tx.len(), 0);
    assert_eq!(rx.len(), 0);
    tx.try_send(1).unwrap();
    tx.try_send(2).unwrap();
    assert_eq!(tx.len(), 2);
    assert_eq!(rx.len(), 2);
    assert_eq!(rx.recv().await, Ok(1));
    assert_eq!(tx.len(), 1);
    assert_eq!(rx.len(), 1);
}
