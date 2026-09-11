use core::num::NonZeroU64;
use std::path::Path;

use notify_debouncer_full::{
    DebouncedEvent, new_debouncer,
    notify::{
        EventKind, RecursiveMode,
        event::{CreateKind, ModifyKind, RenameMode},
    },
};
use tokio::fs::{metadata, read_dir};

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
    use std::sync::Arc;

    use hardy_bpa::{
        async_trait,
        cla::{Acceptance, ClaAddress, Segment, Sink, TransferOutcome},
        stream::Receiver,
    };
    use hardy_bpv7::{bundle::Id, eid::NodeId};

    use super::*;

    /// Signals each dispatch through a channel — the forwarder loop is
    /// sequential, so a received signal proves every earlier-queued path
    /// was already fully handled — and answers with a configured verdict.
    struct MockSink {
        verdict: Acceptance,
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
        ) -> hardy_bpa::cla::Result<Acceptance> {
            while let Ok(segment) = stream.recv().await {
                if matches!(segment, Segment::Final(_)) {
                    break;
                }
            }
            let _ = self.dispatched.send(());
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
                verdict: Acceptance::Accepted,
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

        let (event_tx, _event_rx) = flume::unbounded();
        forwarder_task(
            Arc::new(MockSink {
                verdict: Acceptance::Refused,
                dispatched: event_tx,
            }),
            path_rx,
            None,
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

        // Drive the real restart sweep over the post-quarantine outbox. A
        // sentinel file proves the sweep ran; the quarantined file must not
        // be offered alongside it.
        let sentinel = dir.path().join("sentinel.bundle");
        tokio::fs::write(&sentinel, b"bundle-bytes").await.unwrap();

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
                .expect("restart sweep never offered the sentinel")
                .unwrap();
        assert_eq!(
            offered, sentinel,
            "the restart sweep must offer only the sentinel, never the quarantined file"
        );

        // The watcher is gone once awaited (its sender dropped with it), so
        // the channel holds everything it ever offered: nothing may follow
        // the sentinel.
        cancel.cancel();
        task.await.unwrap();
        assert!(
            path_rx.try_recv().is_err(),
            "the quarantined file must not be re-offered by the restart sweep"
        );
    }
}
