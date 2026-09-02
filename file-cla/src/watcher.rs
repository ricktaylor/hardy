use core::num::NonZeroU64;

use notify_debouncer_full::{
    DebouncedEvent, new_debouncer,
    notify::{EventKind, RecursiveMode, event::CreateKind},
};
use tokio::fs::{metadata, read_dir};

use super::*;

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
    /// * `max_bundle_size` - The negotiated dispatch size cap, if any;
    ///   files larger than it are skipped rather than offered to a certain
    ///   rejection.
    pub async fn start_watcher(
        &self,
        sink: Arc<dyn hardy_bpa::cla::Sink>,
        outbox: String,
        max_bundle_size: Option<NonZeroU64>,
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

    // Dispatch files already queued in the outbox: bundles are routinely
    // dropped in the directory while the CLA is not running (removable media,
    // restarts), and the watcher only reports files created after the watch is
    // installed. The watch is installed before this scan, so a file arriving
    // mid-scan is seen by at least one of the two paths; the forwarder
    // tolerates the resulting duplicate.
    match read_dir(&outbox).await {
        Ok(mut entries) => loop {
            match entries.next_entry().await {
                Ok(Some(entry)) => {
                    // `metadata` follows symlinks where `DirEntry::file_type`
                    // does not: inotify reports the creation of a symlink as
                    // an ordinary file creation, so the scan has to accept one
                    // too, or the two paths disagree about the same entry.
                    let path = entry.path();
                    if metadata(&path).await.is_ok_and(|m| m.is_file())
                        && path_tx.send_async(path).await.is_err()
                    {
                        return;
                    }
                }
                Ok(None) => break,
                Err(e) => {
                    error!("Failed to scan outbox '{outbox}': {e}");
                    break;
                }
            }
        },
        Err(e) => error!("Failed to scan outbox '{outbox}': {e}"),
    }

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
    max_bundle_size: Option<NonZeroU64>,
    cancel_token: tokio_util::sync::CancellationToken,
) {
    loop {
        tokio::select! {
            res = rx.recv_async() => match res {
                Err(_) => break,
                Ok(path) => {
                    // Pre-check against the BPA's dispatch size cap: an
                    // over-cap file would be rejected deterministically, so
                    // don't even read it. Skipped (not deleted) — the
                    // operator's file, the operator's cleanup.
                    if let Some(cap) = max_bundle_size
                        && let Ok(meta) = tokio::fs::metadata(&path).await
                        && meta.len() > cap.get()
                    {
                        warn!("'{}' exceeds the negotiated max bundle size ({} > {cap}), skipped", path.display(), meta.len());
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

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use hardy_bpa::{
        async_trait,
        cla::{ClaAddress, Segment, Sink, TransferOutcome},
        stream::Receiver,
    };
    use hardy_bpv7::{bundle::Id, eid::NodeId};

    use super::*;

    /// Signals each dispatch through a channel; the forwarder loop is
    /// sequential, so a received signal proves every earlier-queued path
    /// was already fully handled.
    struct MockSink {
        dispatched: flume::Sender<()>,
    }

    #[async_trait]
    impl Sink for MockSink {
        async fn unregister(&self) {}

        async fn dispatch(
            &self,
            _peer_node: Option<&NodeId>,
            _peer_addr: Option<&ClaAddress>,
            stream: &mut dyn Receiver<Segment>,
        ) -> hardy_bpa::cla::Result<()> {
            while let Ok(segment) = stream.recv().await {
                if matches!(segment, Segment::Final(_)) {
                    break;
                }
            }
            let _ = self.dispatched.send(());
            Ok(())
        }

        async fn add_peer(
            &self,
            _cla_addr: ClaAddress,
            _node_ids: &[NodeId],
        ) -> hardy_bpa::cla::Result<bool> {
            Ok(true)
        }

        async fn remove_peer(&self, _cla_addr: &ClaAddress) -> hardy_bpa::cla::Result<bool> {
            Ok(true)
        }

        async fn transfer_outcome(
            &self,
            _bundle_id: &Id,
            _outcome: TransferOutcome,
        ) -> hardy_bpa::cla::Result<()> {
            Ok(())
        }
    }

    /// An over-cap outbox file is skipped unread — never dispatched, never
    /// deleted — while a later under-cap file dispatches. The dispatch
    /// channel is FIFO and the forwarder sequential, so the single received
    /// signal is the under-cap file's: an over-cap dispatch would have
    /// queued a signal ahead of it.
    #[tokio::test]
    async fn over_cap_file_is_skipped_unread_and_preserved() {
        let dir = std::env::temp_dir().join(format!("hardy-file-cla-skip-{}", std::process::id()));
        tokio::fs::create_dir_all(&dir).await.unwrap();
        let over = dir.join("over.bundle");
        let under = dir.join("under.bundle");
        // Contents are immaterial: the over-cap file must be skipped on
        // metadata alone, and the mock sink accepts anything.
        tokio::fs::write(&over, [0u8; 64]).await.unwrap();
        tokio::fs::write(&under, [0u8; 8]).await.unwrap();

        let cap = NonZeroU64::new(32).unwrap();
        let (path_tx, path_rx) = flume::unbounded();
        let (event_tx, event_rx) = flume::unbounded();
        let cancel_token = tokio_util::sync::CancellationToken::new();
        let forwarder = tokio::spawn(forwarder_task(
            Arc::new(MockSink {
                dispatched: event_tx,
            }),
            path_rx,
            Some(cap),
            cancel_token.clone(),
        ));

        path_tx.send(over.clone()).unwrap();
        path_tx.send(under.clone()).unwrap();

        event_rx
            .recv_async()
            .await
            .expect("The under-cap file should dispatch");
        assert!(
            event_rx.is_empty(),
            "Only the under-cap file may dispatch; an over-cap dispatch would have signalled first"
        );
        assert!(
            tokio::fs::try_exists(&over).await.unwrap(),
            "The over-cap file is the operator's to clean up"
        );

        cancel_token.cancel();
        forwarder.await.unwrap();
        tokio::fs::remove_dir_all(&dir).await.unwrap();
    }
}
