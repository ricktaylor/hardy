//! File watcher with debouncing.
//!
//! Monitors a single file for changes (create, modify, remove) and calls
//! a callback. Supports native OS events and periodic polling (for Docker).

use core::{future::Future, time::Duration};
use std::path::Path;

use futures::FutureExt;
use notify::{
    EventKind, PollWatcher, RecursiveMode,
    event::{CreateKind, RemoveKind},
};
use notify_debouncer_full::{DebouncedEvent, RecommendedCache, new_debouncer_opt};
use trace_err::*;
use tracing::error;

use crate::CancellationToken;

/// How to detect file changes.
#[derive(Clone, Copy, Debug)]
pub enum WatchMode {
    /// OS-native events (inotify/kqueue). Fast but does not work across Docker bind mounts.
    Native,
    /// Periodic polling. Works everywhere including Docker bind mounts (~2s latency).
    Poll,
}

/// Watches a file for changes and calls `on_change` when it is created,
/// modified, or removed. Uses a 1-second debounce to coalesce rapid writes.
///
/// The watcher monitors the file's parent directory (non-recursive) and
/// filters events to only the target file. Runs until `cancel` is triggered.
pub async fn watch<F, Fut>(path: &Path, mode: WatchMode, cancel: CancellationToken, on_change: F)
where
    F: Fn() -> Fut + Send + 'static,
    Fut: Future<Output = ()>,
{
    let watch_dir = path
        .parent()
        .expect("watched file has no parent directory")
        .to_path_buf();
    let watch_file = path.to_path_buf();

    let (tx, rx) = flume::unbounded();
    let callback = move |res| match res {
        Ok(events) => {
            for e in events {
                if tx.send(e).is_err() {
                    break;
                }
            }
        }
        Err(errors) => {
            for e in errors {
                error!("File watch error: {e}");
            }
        }
    };

    let debounce_timeout = Duration::from_secs(1);

    match mode {
        WatchMode::Native => {
            let mut debouncer =
                new_debouncer_opt::<_, notify::RecommendedWatcher, RecommendedCache>(
                    debounce_timeout,
                    None,
                    callback,
                    RecommendedCache::new(),
                    notify::Config::default(),
                )
                .trace_expect("Failed to create file watcher");

            debouncer
                .watch(&watch_dir, RecursiveMode::NonRecursive)
                .trace_expect("Failed to watch directory");

            watch_loop(&watch_file, &rx, &cancel, &on_change).await;
        }
        WatchMode::Poll => {
            let poll_config = notify::Config::default().with_poll_interval(Duration::from_secs(2));
            let mut debouncer = new_debouncer_opt::<_, PollWatcher, RecommendedCache>(
                debounce_timeout,
                None,
                callback,
                RecommendedCache::new(),
                poll_config,
            )
            .trace_expect("Failed to create file watcher");

            debouncer
                .watch(&watch_dir, RecursiveMode::NonRecursive)
                .trace_expect("Failed to watch directory");

            watch_loop(&watch_file, &rx, &cancel, &on_change).await;
        }
    }
}

async fn watch_loop<F, Fut>(
    watch_file: &Path,
    rx: &flume::Receiver<DebouncedEvent>,
    cancel: &CancellationToken,
    on_change: &F,
) where
    F: Fn() -> Fut,
    Fut: Future<Output = ()>,
{
    loop {
        let recv = rx.recv_async();
        futures::pin_mut!(recv);
        futures::select_biased! {
            res = recv.fuse() => match res {
                Err(_) => break,
                Ok(DebouncedEvent { event, .. }) => {
                    let relevant = matches!(
                        event.kind,
                        EventKind::Create(CreateKind::File)
                        | EventKind::Modify(_)
                        | EventKind::Remove(RemoveKind::File)
                    ) && event.paths.iter().any(|p| p == watch_file);

                    if relevant {
                        on_change().await;
                    }
                }
            },
            _ = cancel.cancelled().fuse() => break,
        }
    }
}

#[cfg(all(test, feature = "tokio"))]
mod tests {
    use std::{
        path::PathBuf,
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
        time::Instant,
    };

    use notify::{
        Event,
        event::{AccessKind, ModifyKind},
    };

    use super::*;

    fn event(kind: EventKind, path: &Path) -> DebouncedEvent {
        DebouncedEvent::new(
            Event::new(kind).add_path(path.to_path_buf()),
            Instant::now(),
        )
    }

    fn counting_callback() -> (Arc<AtomicUsize>, impl Fn() -> futures::future::Ready<()>) {
        let count = Arc::new(AtomicUsize::new(0));
        let counter = count.clone();
        (count, move || {
            counter.fetch_add(1, Ordering::SeqCst);
            futures::future::ready(())
        })
    }

    /// `watch_loop` drains ready events before it polls the cancel arm
    /// (`select_biased!` with the receive arm first), so pre-loading the
    /// channel and pre-cancelling the token processes the whole backlog and
    /// then breaks, deterministically.
    #[tokio::test]
    async fn watch_loop_filters_kind_and_path() {
        let watched = PathBuf::from("/tmp/watched/config.toml");
        let sibling = PathBuf::from("/tmp/watched/other.toml");

        let (tx, rx) = flume::unbounded();

        // Relevant: file create, any modify, and file remove of the watched path.
        tx.send(event(EventKind::Modify(ModifyKind::Any), &watched))
            .unwrap();
        tx.send(event(EventKind::Create(CreateKind::File), &watched))
            .unwrap();
        tx.send(event(EventKind::Remove(RemoveKind::File), &watched))
            .unwrap();

        // Ignored: right kind, wrong path.
        tx.send(event(EventKind::Modify(ModifyKind::Any), &sibling))
            .unwrap();

        // Ignored: wrong kind, right path.
        tx.send(event(EventKind::Access(AccessKind::Any), &watched))
            .unwrap();
        tx.send(event(EventKind::Create(CreateKind::Folder), &watched))
            .unwrap();
        tx.send(event(EventKind::Remove(RemoveKind::Folder), &watched))
            .unwrap();

        let cancel = CancellationToken::new();
        cancel.cancel();

        let (count, on_change) = counting_callback();
        watch_loop(&watched, &rx, &cancel, &on_change).await;

        assert_eq!(count.load(Ordering::SeqCst), 3);
    }

    /// Dropping the sender ends the loop via the receive error arm.
    #[tokio::test]
    async fn watch_loop_ends_when_sender_dropped() {
        let watched = PathBuf::from("/tmp/watched/config.toml");

        let (tx, rx) = flume::unbounded();
        tx.send(event(EventKind::Modify(ModifyKind::Any), &watched))
            .unwrap();
        drop(tx);

        let (count, on_change) = counting_callback();
        watch_loop(&watched, &rx, &CancellationToken::new(), &on_change).await;

        assert_eq!(count.load(Ordering::SeqCst), 1);
    }

    /// Cancellation ends the loop even while the sender is still alive.
    #[tokio::test]
    async fn watch_loop_ends_on_cancel_with_live_sender() {
        let watched = PathBuf::from("/tmp/watched/config.toml");

        let (tx, rx) = flume::unbounded::<DebouncedEvent>();
        let cancel = CancellationToken::new();
        cancel.cancel();

        let (count, on_change) = counting_callback();
        watch_loop(&watched, &rx, &cancel, &on_change).await;

        assert_eq!(count.load(Ordering::SeqCst), 0);
        drop(tx);
    }
}
