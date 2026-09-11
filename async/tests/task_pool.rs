#![cfg(feature = "tokio")]

use hardy_async::TaskPool;

#[tokio::test]
async fn test_task_pool_spawn_and_shutdown() {
    let pool = TaskPool::new();
    let cancel = pool.cancel_token().clone();

    let handle = pool.spawn(async move {
        cancel.cancelled().await;
        42
    });

    pool.shutdown().await;
    assert!(pool.is_cancelled());

    // The task ran, observed cancellation, and its output is retrievable.
    assert_eq!(handle.await.unwrap(), 42);
}

/// Pins the documented spawn-after-shutdown semantics: the tracker does
/// not reject spawns after close, so the task still runs and its handle
/// is awaitable, even though the completed shutdown() never waited for
/// it. If spawning after shutdown is ever made to panic instead, this
/// test must change with the documentation.
#[tokio::test]
async fn test_spawn_after_shutdown_still_runs_task() {
    let pool = TaskPool::new();
    pool.shutdown().await;

    let handle = pool.spawn(async { 42 });
    assert_eq!(handle.await.unwrap(), 42);
}

#[tokio::test]
async fn test_child_token_independent_cancellation() {
    let pool = TaskPool::new();
    let child = pool.child_token();

    // Cancel child without affecting parent
    child.cancel();

    assert!(child.is_cancelled());
    assert!(!pool.is_cancelled());
}

#[tokio::test]
async fn test_parent_cancels_child() {
    let pool = TaskPool::new();
    let child = pool.child_token();

    // Cancel parent
    pool.shutdown().await;

    // Child is also cancelled
    assert!(child.is_cancelled());
    assert!(pool.is_cancelled());
}
