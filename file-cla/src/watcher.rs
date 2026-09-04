use super::*;
use notify_debouncer_full::{
    DebouncedEvent, new_debouncer,
    notify::{EventKind, RecursiveMode, event::CreateKind},
};

impl Cla {
    /// Starts the file watcher for the outbox directory.
    ///
    /// This function spawns two background tasks:
    /// 1. `watcher_task`: Monitors the `outbox` directory for new files. When a new
    ///    file is created, its path is sent to the `forwarder_task`.
    /// 2. `forwarder_task`: Receives file paths, reads the file content as a bundle,
    ///    dispatches it to the BPA via the `sink`, and then deletes the file.
    ///
    /// # Arguments
    ///
    /// * `sink` - The sink to dispatch bundles to the BPA.
    /// * `outbox` - The path to the directory to watch for outgoing bundles.
    /// * `max_bundle_size` - The BPA's dispatch size cap; files larger than
    ///   it are skipped rather than offered to a certain refusal.
    pub async fn start_watcher(
        &self,
        sink: Arc<dyn hardy_bpa::cla::Sink>,
        outbox: String,
        max_bundle_size: core::num::NonZeroU64,
    ) {
        let (path_tx, path_rx) = flume::unbounded::<PathBuf>();

        let cancel_token = self.tasks.cancel_token().clone();
        hardy_async::spawn!(self.tasks, "watcher_task", async move {
            watcher_task(outbox, path_tx, cancel_token).await
        });

        let cancel_token = self.tasks.cancel_token().clone();
        hardy_async::spawn!(self.tasks, "forwarder_task", async move {
            forwarder_task(sink, path_rx, max_bundle_size, cancel_token).await
        });
    }
}

async fn watcher_task(
    outbox: String,
    path_tx: flume::Sender<PathBuf>,
    cancel_token: tokio_util::sync::CancellationToken,
) {
    let (tx, rx) = flume::unbounded();
    let mut debouncer = new_debouncer(
        std::time::Duration::from_secs(1),
        None,
        move |res| match res {
            Ok(events) => {
                for e in events {
                    if tx.send(e).is_err() {
                        break;
                    }
                }
            }
            Err(e) => {
                for e in e {
                    error!("Watch error: {e}")
                }
            }
        },
    )
    .trace_expect("Failed to create directory watcher");

    debouncer
        .watch(&outbox, RecursiveMode::NonRecursive)
        .trace_expect("Failed to watch file");

    info!("Watching '{outbox}' for new files");

    loop {
        tokio::select! {
            res = rx.recv_async() => match res {
                Err(_) => break,
                Ok(DebouncedEvent{ event, .. }) => {
                    if event.kind == EventKind::Create(CreateKind::File) {
                        for e in event.paths {
                            if path_tx.send_async(e).await.is_err() {
                                break;
                            }
                        }
                    }

                },
            },
            _ = cancel_token.cancelled() => {
                break;
            }
        }
    }
}

async fn forwarder_task(
    sink: Arc<dyn hardy_bpa::cla::Sink>,
    rx: flume::Receiver<PathBuf>,
    max_bundle_size: core::num::NonZeroU64,
    cancel_token: tokio_util::sync::CancellationToken,
) {
    loop {
        tokio::select! {
            res = rx.recv_async() => match res {
                Err(_) => break,
                Ok(path) => {
                    // Pre-check against the BPA's dispatch size cap: an
                    // over-cap file would be refused deterministically, so
                    // don't even read it. Skipped (not deleted) — the
                    // operator's file, the operator's cleanup.
                    if let Ok(meta) = tokio::fs::metadata(&path).await
                        && meta.len() > max_bundle_size.get()
                    {
                        warn!("'{}' exceeds the BPA's max bundle size ({} > {max_bundle_size}), skipped", path.display(), meta.len());
                        continue;
                    }

                    // INTERIM BUFFERING: the whole file is read into memory and
                    // dispatched as a one-segment stream (`Bytes` is a `stream::Receiver`). This
                    // is a deliberate stepping stone toward the full streaming
                    // pipeline (a native implementation would stream the file in
                    // chunks); see bpa/docs/streaming_pipeline_design.md.
                    if let Ok(buffer) = tokio::fs::read(&path).await.inspect_err(|e| error!("Failed to read from '{}': {e}", path.display())) {
                        // The file is consumed only on acceptance: a refused
                        // or failed dispatch leaves it in place for the next
                        // scan, since the BPA has not taken responsibility
                        // for the bundle.
                        // TODO:  We could implement a "Sent Items" folder instead of deleting, but not sure...
                        match sink.dispatch(None, None, &mut hardy_bpa::Bytes::from(buffer)).await {
                            Ok(hardy_bpa::cla::Acceptance::Accepted) => {
                                debug!("Dispatched '{}'", path.display());
                                tokio::fs::remove_file(&path).await.unwrap_or_else(|e| {
                                    warn!("Failed to remove file '{}': {e}", path.display());
                                });
                            }
                            Ok(hardy_bpa::cla::Acceptance::Refused) => {
                                warn!("Bundle '{}' refused by the BPA, file left in place", path.display());
                            }
                            Err(e) => warn!("Failed to dispatch bundle '{}': {e}", path.display()),
                        }
                    }
                }
            },
            _ = cancel_token.cancelled() => {
                break;
            }
        }
    }
}
