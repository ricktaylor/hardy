#![cfg(feature = "tokio")]

use core::{
    sync::atomic::{AtomicUsize, Ordering},
    time::Duration,
};
use std::sync::Arc;

use hardy_async::BoundedTaskPool;

// Spawns `tasks` tasks on `pool` that rendezvous in groups of `group` and
// returns the concurrency high-water mark observed across them. Each task
// holds its slot until `group` tasks are running at once, so with a pool
// limit of `group` the returned mark is provably exactly `group`: the
// barrier forces it to be reached, and the pool's semaphore keeps it from
// being exceeded (slots are released before permits are returned).
async fn concurrency_high_water_mark(pool: &BoundedTaskPool, tasks: usize, group: usize) -> usize {
    let concurrent = Arc::new(AtomicUsize::new(0));
    let max_concurrent = Arc::new(AtomicUsize::new(0));
    let barrier = Arc::new(tokio::sync::Barrier::new(group));

    let mut handles = vec![];
    for _ in 0..tasks {
        let concurrent = concurrent.clone();
        let max_concurrent = max_concurrent.clone();
        let barrier = barrier.clone();

        handles.push(
            pool.spawn(async move {
                let current = concurrent.fetch_add(1, Ordering::SeqCst) + 1;
                max_concurrent.fetch_max(current, Ordering::SeqCst);
                barrier.wait().await;
                concurrent.fetch_sub(1, Ordering::SeqCst);
            })
            .await,
        );
    }

    for handle in handles {
        handle.await.unwrap();
    }

    max_concurrent.load(Ordering::SeqCst)
}

#[tokio::test]
async fn test_bounded_pool_limits_concurrency() {
    let pool = BoundedTaskPool::new(core::num::NonZeroUsize::new(2).unwrap());
    assert_eq!(concurrency_high_water_mark(&pool, 10, 2).await, 2);
}

#[tokio::test]
async fn test_bounded_pool_default_uses_available_parallelism() {
    let pool = BoundedTaskPool::default();
    let expected: usize = hardy_async::available_parallelism().into();

    // The default limit is available_parallelism: the pool admits exactly
    // `expected` tasks at once.
    assert_eq!(
        concurrency_high_water_mark(&pool, expected * 2, expected).await,
        expected
    );

    pool.shutdown().await;
    assert!(pool.is_cancelled());
}

#[tokio::test]
async fn test_bounded_child_token_independent_cancellation() {
    let pool = BoundedTaskPool::new(core::num::NonZeroUsize::new(2).unwrap());
    let child = pool.child_token();

    // Cancel child without affecting parent
    child.cancel();

    assert!(child.is_cancelled());
    assert!(!pool.is_cancelled());
}

#[tokio::test]
async fn test_bounded_parent_cancels_child() {
    let pool = BoundedTaskPool::new(core::num::NonZeroUsize::new(2).unwrap());
    let child = pool.child_token();

    // Cancel parent
    pool.shutdown().await;

    // Child is also cancelled
    assert!(child.is_cancelled());
    assert!(pool.is_cancelled());
}

#[tokio::test]
async fn test_bounded_pool_shutdown() {
    let pool = BoundedTaskPool::new(core::num::NonZeroUsize::new(4).unwrap());
    let completed = Arc::new(AtomicUsize::new(0));

    for _ in 0..4 {
        let completed = completed.clone();
        let cancel = pool.cancel_token().clone();

        pool.spawn(async move {
            loop {
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_millis(10)) => {}
                    _ = cancel.cancelled() => {
                        completed.fetch_add(1, Ordering::SeqCst);
                        break;
                    }
                }
            }
        })
        .await;
    }

    pool.shutdown().await;

    // All tasks should have completed
    assert_eq!(completed.load(Ordering::SeqCst), 4);
}
