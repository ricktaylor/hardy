#![cfg(feature = "tokio")]

use hardy_async::{TaskPool, signal::listen_for_cancel};

/// `shutdown()` must complete without any OS signal arriving: the
/// handler's `cancelled()` arm terminates `wait_for_signal`, letting the
/// tracked task finish and the pool drain. The timeout is a failure
/// bound, not a synchronisation point; it only elapses if shutdown hangs.
#[tokio::test]
async fn shutdown_unblocks_signal_handler() {
    let pool = TaskPool::new();
    listen_for_cancel(&pool);

    tokio::time::timeout(core::time::Duration::from_secs(5), pool.shutdown())
        .await
        .expect("shutdown hung waiting for the signal handler task");
}
