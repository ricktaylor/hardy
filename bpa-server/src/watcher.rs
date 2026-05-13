use std::future::Future;
use std::path::Path;
use std::time::Duration;

use hardy_async::CancellationToken;
use notify_debouncer_full::notify::event::{CreateKind, RemoveKind};
use notify_debouncer_full::notify::{EventKind, RecursiveMode};
use notify_debouncer_full::{DebouncedEvent, new_debouncer};
use trace_err::TraceErrResult;
use tracing::error;

/// Watches a file for changes and calls `on_change` when it is created,
/// modified, or removed. Uses a 1-second debounce to coalesce rapid writes.
///
/// The watcher monitors the file's parent directory (non-recursive) and
/// filters events to only the target file. Runs until `cancel` is triggered.
pub async fn watch<F, Fut>(path: &Path, cancel: CancellationToken, on_change: F)
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
    let mut debouncer = new_debouncer(Duration::from_secs(1), None, move |res| match res {
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
    })
    .trace_expect("Failed to create file watcher");

    debouncer
        .watch(&watch_dir, RecursiveMode::NonRecursive)
        .trace_expect("Failed to watch directory");

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
                    ) && event.paths.iter().any(|p| p == &watch_file);

                    if relevant {
                        on_change().await;
                    }
                }
            },
            _ = cancel.cancelled() => break,
        }
    }
}
