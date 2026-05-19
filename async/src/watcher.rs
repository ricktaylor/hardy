//! File watcher with debouncing.
//!
//! Monitors a single file for changes (create, modify, remove) and calls
//! a callback. Supports native OS events and periodic polling (for Docker).

use std::future::Future;
use std::path::Path;
use std::time::Duration;

use notify::event::{CreateKind, RemoveKind};
use notify::{EventKind, PollWatcher, RecursiveMode};
use notify_debouncer_full::{DebouncedEvent, RecommendedCache, new_debouncer_opt};
use serde::{Deserialize, Serialize};
use trace_err::*;
use tracing::error;

use crate::CancellationToken;

/// How to detect file changes.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
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
        tokio::select! {
            res = rx.recv_async() => match res {
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
            _ = cancel.cancelled() => break,
        }
    }
}
