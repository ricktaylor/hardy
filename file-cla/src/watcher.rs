use std::path::Path;

use notify_debouncer_full::{
    DebouncedEvent, new_debouncer,
    notify::{
        EventKind, RecursiveMode,
        event::{CreateKind, ModifyKind, RenameMode},
    },
};

use super::*;

impl Cla {
    /// Starts the file watcher for the outbox directory.
    ///
    /// This function spawns two background tasks:
    /// 1. `watcher_task`: Sweeps the files already in `outbox` (each is
    ///    offered once at startup), then monitors the directory for created
    ///    or renamed-in files, sending each path to the `forwarder_task`.
    /// 2. `forwarder_task`: Receives file paths, reads each file as a
    ///    bundle and dispatches it to the BPA via the `sink`. The file is
    ///    deleted on acceptance, quarantined to the `outbox/refused/`
    ///    subdirectory on refusal (a refusal is deterministic — re-offering
    ///    would refuse forever), and left in place on a dispatch failure so
    ///    the next startup sweep re-offers it.
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

    // Startup sweep: the watcher reacts only to events, so files already in
    // the outbox — present before start, or left in place by a dispatch
    // failure before a restart — would otherwise never be offered. Watch
    // first, then sweep: no file is missed, and a file created mid-sweep may
    // be offered twice, which the serial forwarder tolerates. Directory
    // entries (`refused/`) are skipped.
    match tokio::fs::read_dir(&outbox).await {
        Ok(mut entries) => loop {
            match entries.next_entry().await {
                Ok(Some(entry)) => {
                    if entry.file_type().await.is_ok_and(|t| t.is_file())
                        && path_tx.send_async(entry.path()).await.is_err()
                    {
                        return;
                    }
                }
                Ok(None) => break,
                Err(e) => {
                    warn!("Failed to sweep '{outbox}': {e}");
                    break;
                }
            }
        },
        Err(e) => warn!("Failed to sweep '{outbox}': {e}"),
    }

    info!("Watching '{outbox}' for new files");

    loop {
        tokio::select! {
            res = rx.recv_async() => match res {
                Err(_) => break,
                Ok(DebouncedEvent{ event, .. }) => {
                    // Create catches plain writes; rename-into catches the
                    // atomic write-then-rename spool idiom and an operator
                    // moving a file back from `refused/` to retry it.
                    if matches!(
                        event.kind,
                        EventKind::Create(CreateKind::File)
                            | EventKind::Modify(ModifyKind::Name(RenameMode::To | RenameMode::Any))
                    ) {
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

// A refusal is deterministic — re-offering the same bundle would refuse
// forever — so the file moves to the `refused/` sibling directory: the
// non-recursive sweep and watcher never re-offer it, and the operator can
// inspect it or `mv` it back into the outbox to retry. On any failure the
// file is left in place.
async fn quarantine(path: &Path) {
    let (Some(dir), Some(name)) = (path.parent(), path.file_name()) else {
        warn!(
            "Bundle '{}' refused by the BPA, file left in place",
            path.display()
        );
        return;
    };
    let refused = dir.join("refused");
    if let Err(e) = tokio::fs::create_dir_all(&refused).await {
        warn!(
            "Bundle '{}' refused by the BPA; creating '{}' failed ({e}), file left in place",
            path.display(),
            refused.display()
        );
        return;
    }
    let dest = refused.join(name);
    match tokio::fs::rename(path, &dest).await {
        Ok(()) => warn!(
            "Bundle '{}' refused by the BPA, quarantined at '{}'",
            path.display(),
            dest.display()
        ),
        Err(e) => warn!(
            "Bundle '{}' refused by the BPA; quarantine failed ({e}), file left in place",
            path.display()
        ),
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
                        // The file is consumed only on acceptance. A refusal
                        // is deterministic and quarantined to `refused/`; a
                        // dispatch failure is transient, so the file stays
                        // for the next startup sweep to re-offer.
                        // TODO:  We could implement a "Sent Items" folder instead of deleting, but not sure...
                        match sink.dispatch(None, None, &mut hardy_bpa::Bytes::from(buffer)).await {
                            Ok(hardy_bpa::cla::Acceptance::Accepted) => {
                                debug!("Dispatched '{}'", path.display());
                                tokio::fs::remove_file(&path).await.unwrap_or_else(|e| {
                                    warn!("Failed to remove file '{}': {e}", path.display());
                                });
                            }
                            Ok(hardy_bpa::cla::Acceptance::Refused) => {
                                quarantine(&path).await;
                            }
                            Err(e) => warn!("Failed to dispatch bundle '{}': {e}; file left in place — the startup sweep re-offers it after restart", path.display()),
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
    use hardy_bpa::cla::{Acceptance, ClaAddress, Sink, TransferOutcome};
    use hardy_bpv7::{bundle::Id, eid::NodeId};

    use super::*;

    struct StubSink {
        verdict: Acceptance,
    }

    #[hardy_bpa::async_trait]
    impl Sink for StubSink {
        async fn unregister(&self) {}

        async fn dispatch(
            &self,
            _peer_node: Option<&NodeId>,
            _peer_addr: Option<&ClaAddress>,
            stream: &mut dyn hardy_bpa::stream::Receiver<hardy_bpa::stream::Segment>,
        ) -> hardy_bpa::cla::Result<Acceptance> {
            while let Ok(segment) = stream.recv().await {
                if matches!(segment, hardy_bpa::stream::Segment::Final(_)) {
                    break;
                }
            }
            Ok(self.verdict)
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

    // A file already in the outbox when the watcher starts is offered by
    // the startup sweep — previously nothing ever offered it.
    #[tokio::test]
    async fn startup_sweep_offers_preexisting_files() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("pre-existing.bundle");
        tokio::fs::write(&file, b"bundle-bytes").await.unwrap();

        let (path_tx, path_rx) = flume::unbounded::<PathBuf>();
        let cancel = tokio_util::sync::CancellationToken::new();
        let task = tokio::spawn(watcher_task(
            dir.path().to_string_lossy().into_owned(),
            path_tx,
            cancel.clone(),
        ));

        // the timeout only bounds a regression
        let offered =
            tokio::time::timeout(std::time::Duration::from_secs(10), path_rx.recv_async())
                .await
                .expect("startup sweep never offered the pre-existing file")
                .unwrap();
        assert_eq!(offered, file);

        cancel.cancel();
        task.await.unwrap();
    }

    // A refused file is quarantined under `refused/`, and a subsequent
    // sweep does not re-offer it (the quarantine directory is not a
    // regular file, and its contents are outside the non-recursive scan).
    #[tokio::test]
    async fn refused_file_is_quarantined_not_reoffered() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("refused.bundle");
        tokio::fs::write(&file, b"bundle-bytes").await.unwrap();

        let (path_tx, path_rx) = flume::unbounded::<PathBuf>();
        path_tx.send(file.clone()).unwrap();
        // Dropping the sender ends the forwarder loop after the one offer,
        // so awaiting it is the completion barrier.
        drop(path_tx);

        forwarder_task(
            Arc::new(StubSink {
                verdict: Acceptance::Refused,
            }),
            path_rx,
            core::num::NonZeroU64::new(u64::MAX).unwrap(),
            tokio_util::sync::CancellationToken::new(),
        )
        .await;

        assert!(
            tokio::fs::try_exists(dir.path().join("refused").join("refused.bundle"))
                .await
                .unwrap(),
            "refused file must be quarantined under refused/"
        );
        assert!(
            !tokio::fs::try_exists(&file).await.unwrap(),
            "refused file must leave the outbox"
        );

        // The sweep's own filter: no regular file remains in the outbox,
        // so a restart would not re-offer the refusal.
        let mut entries = tokio::fs::read_dir(dir.path()).await.unwrap();
        while let Some(entry) = entries.next_entry().await.unwrap() {
            assert!(
                !entry.file_type().await.unwrap().is_file(),
                "outbox must hold no re-offerable files after quarantine"
            );
        }
    }
}
