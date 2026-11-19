use core::num::NonZeroU64;
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use flume::{Receiver, Sender};
use hardy_async::CancellationToken;
use hardy_bpa::{Bytes, cla};
use notify_debouncer_full::{
    DebouncedEvent, new_debouncer,
    notify::{
        Event, EventKind, RecursiveMode,
        event::{CreateKind, ModifyKind, RenameMode},
    },
};
use tokio::fs;
use trace_err::TraceErrResult;
use tracing::{debug, error, info, warn};

use crate::Cla;

impl Cla {
    /// Spawns the outbox watcher and the forwarder that drains it.
    ///
    /// The watcher sweeps the files already in `outbox`, then follows the
    /// directory for files created or renamed into it, handing each path to
    /// the forwarder. The forwarder reads a file whole and dispatches it to
    /// the BPA through `sink`: an accepted file is deleted, a refused one is
    /// quarantined under `outbox/refused/`, and one whose dispatch failed is
    /// left in place for the next startup sweep to re-offer.
    ///
    /// Files larger than `max_bundle_size` are skipped rather than offered to
    /// a certain rejection.
    ///
    /// Both tasks run until the CLA's task pool is cancelled.
    pub async fn start_watcher(
        &self,
        sink: Arc<dyn cla::Sink>,
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

async fn watcher_task(outbox: String, path_tx: Sender<PathBuf>, cancel_token: CancellationToken) {
    let (tx, rx) = flume::unbounded();
    let mut debouncer = new_debouncer(Duration::from_secs(1), None, move |res| match res {
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
    })
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
    match fs::read_dir(&outbox).await {
        Ok(mut entries) => loop {
            match entries.next_entry().await {
                Ok(Some(entry)) => {
                    // `metadata` follows symlinks where `DirEntry::file_type`
                    // does not: inotify reports the creation of a symlink as
                    // an ordinary file creation, so the scan has to accept one
                    // too, or the two paths disagree about the same entry.
                    let path = entry.path();
                    if fs::metadata(&path).await.is_ok_and(|m| m.is_file())
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
                    for e in offered_paths(event) {
                        if path_tx.send_async(e).await.is_err() {
                            break;
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

// The outbox paths an FS event offers. Create catches plain writes;
// rename-into catches the atomic write-then-rename spool idiom and an
// operator moving a file back from `refused/` to retry it. A rename the
// debouncer matched within the directory (the source outlived the debounce
// window) arrives as a single `Both` event whose paths are `[from, to]`, of
// which only the destination still exists to be offered.
fn offered_paths(mut event: Event) -> Vec<PathBuf> {
    match event.kind {
        EventKind::Create(CreateKind::File)
        | EventKind::Modify(ModifyKind::Name(RenameMode::To | RenameMode::Any)) => event.paths,
        EventKind::Modify(ModifyKind::Name(RenameMode::Both)) => {
            event.paths.pop().into_iter().collect()
        }
        _ => Vec::new(),
    }
}

// Moves a refused file to the `refused/` sibling directory, which neither
// the non-recursive sweep nor the watcher scans, so the same refusal is not
// repeated on every restart. The operator can inspect the file or `mv` it
// back into the outbox to retry. On any failure the file is left in place.
async fn quarantine(path: &Path) {
    let (Some(dir), Some(name)) = (path.parent(), path.file_name()) else {
        warn!(
            "Bundle '{}' refused by the BPA, file left in place",
            path.display()
        );
        return;
    };
    let refused = dir.join("refused");
    if let Err(e) = fs::create_dir_all(&refused).await {
        warn!(
            "Bundle '{}' refused by the BPA; creating '{}' failed ({e}), file left in place",
            path.display(),
            refused.display()
        );
        return;
    }
    let dest = refused.join(name);
    match fs::rename(path, &dest).await {
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
    sink: Arc<dyn cla::Sink>,
    rx: Receiver<PathBuf>,
    max_bundle_size: Option<NonZeroU64>,
    cancel_token: CancellationToken,
) {
    loop {
        tokio::select! {
            res = rx.recv_async() => match res {
                Err(_) => break,
                Ok(path) => {
                    // An over-cap file is rejected deterministically, so it
                    // is never read. It is skipped rather than deleted: the
                    // operator owns the file.
                    if let Some(cap) = max_bundle_size
                        && let Ok(meta) = fs::metadata(&path).await
                        && meta.len() > cap.get()
                    {
                        warn!("'{}' exceeds the negotiated max bundle size ({} > {cap}), skipped", path.display(), meta.len());
                        continue;
                    }

                    // The whole file is read into memory and dispatched as a
                    // one-segment stream, since `Bytes` is a
                    // `stream::Receiver`. See
                    // bpa/docs/streaming_pipeline_design.md for the chunked
                    // form this is a step towards.
                    if let Ok(buffer) = fs::read(&path).await.inspect_err(|e| error!("Failed to read from '{}': {e}", path.display())) {
                        // The file is consumed only on acceptance. A refusal
                        // is deterministic and quarantined to `refused/`; a
                        // dispatch failure is transient, so the file stays
                        // for the next startup sweep to re-offer.
                        // TODO: a "Sent Items" folder could replace the delete.
                        match sink.dispatch(None, None, &mut Bytes::from(buffer)).await {
                            Ok(cla::Acceptance::Accepted) => {
                                debug!("Dispatched '{}'", path.display());
                                fs::remove_file(&path).await.unwrap_or_else(|e| {
                                    warn!("Failed to remove file '{}': {e}", path.display());
                                });
                            }
                            Ok(cla::Acceptance::Refused) => {
                                quarantine(&path).await;
                            }
                            Err(e) => warn!("Failed to dispatch bundle '{}': {e}; file left in place for the startup sweep to re-offer after restart", path.display()),
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
        cla::{Acceptance, Address, Segment, Sink, TransferOutcome},
        stream::Receiver,
    };
    use hardy_bpv7::{bundle::Id, eid::NodeId};
    use notify_debouncer_full::notify::event::DataChange;

    use super::*;

    /// A rename the debouncer matched within the outbox
    /// (`RenameMode::Both`, the write-then-rename idiom when the source
    /// outlived the debounce window) offers only the destination: the
    /// source path no longer exists.
    #[test]
    fn matched_rename_offers_only_the_destination() {
        let offered = offered_paths(
            Event::new(EventKind::Modify(ModifyKind::Name(RenameMode::Both)))
                .add_path(PathBuf::from("/outbox/.bundle.cbor.tmp"))
                .add_path(PathBuf::from("/outbox/bundle.cbor")),
        );
        assert_eq!(offered, vec![PathBuf::from("/outbox/bundle.cbor")]);
    }

    #[test]
    fn create_and_rename_in_offer_the_path() {
        for kind in [
            EventKind::Create(CreateKind::File),
            EventKind::Modify(ModifyKind::Name(RenameMode::To)),
            EventKind::Modify(ModifyKind::Name(RenameMode::Any)),
        ] {
            let offered =
                offered_paths(Event::new(kind).add_path(PathBuf::from("/outbox/bundle.cbor")));
            assert_eq!(
                offered,
                vec![PathBuf::from("/outbox/bundle.cbor")],
                "{kind:?}"
            );
        }
    }

    /// Data-modify chatter and rename-out must not (re-)offer a path.
    #[test]
    fn other_events_offer_nothing() {
        for kind in [
            EventKind::Modify(ModifyKind::Data(DataChange::Any)),
            EventKind::Modify(ModifyKind::Name(RenameMode::From)),
        ] {
            let offered =
                offered_paths(Event::new(kind).add_path(PathBuf::from("/outbox/bundle.cbor")));
            assert_eq!(offered, Vec::<PathBuf>::new(), "{kind:?}");
        }
    }

    /// Answers every dispatch with a configured verdict and signals it
    /// through a channel.
    ///
    /// The forwarder loop is sequential, so a received signal proves every
    /// earlier-queued path was already fully handled.
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
            _peer_addr: Option<&Address>,
            stream: &mut dyn Receiver<Segment>,
        ) -> cla::Result<Acceptance> {
            while let Ok(segment) = stream.recv().await {
                if matches!(segment, Segment::Final(_)) {
                    break;
                }
            }
            let _ = self.dispatched.send(());
            Ok(self.verdict)
        }

        async fn add_peer(&self, _cla_addr: Address, _node_ids: &[NodeId]) -> cla::Result<bool> {
            Ok(true)
        }

        async fn remove_peer(&self, _cla_addr: &Address) -> cla::Result<bool> {
            Ok(true)
        }

        async fn report_transfer(
            &self,
            _bundle_id: &Id,
            _outcome: TransferOutcome,
        ) -> cla::Result<()> {
            Ok(())
        }
    }

    /// An over-cap outbox file is neither dispatched nor deleted, while a
    /// later under-cap file dispatches.
    ///
    /// The dispatch channel is FIFO and the forwarder sequential, so the
    /// single received signal is the under-cap file's: an over-cap dispatch
    /// would have queued a signal ahead of it.
    #[tokio::test]
    async fn over_cap_file_is_skipped_unread_and_preserved() {
        let dir = std::env::temp_dir().join(format!("hardy-file-cla-skip-{}", std::process::id()));
        fs::create_dir_all(&dir).await.unwrap();
        let over = dir.join("over.bundle");
        let under = dir.join("under.bundle");
        // Contents are immaterial: the over-cap file must be skipped on
        // metadata alone, and the mock sink accepts anything.
        fs::write(&over, [0u8; 64]).await.unwrap();
        fs::write(&under, [0u8; 8]).await.unwrap();

        let cap = NonZeroU64::new(32).unwrap();
        let (path_tx, path_rx) = flume::unbounded();
        let (event_tx, event_rx) = flume::unbounded();
        let cancel_token = CancellationToken::new();
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
            fs::try_exists(&over).await.unwrap(),
            "The over-cap file is the operator's to clean up"
        );

        cancel_token.cancel();
        forwarder.await.unwrap();
        fs::remove_dir_all(&dir).await.unwrap();
    }

    // The startup sweep offers a file already in the outbox when the
    // watcher starts.
    #[tokio::test]
    async fn startup_sweep_offers_preexisting_files() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("pre-existing.bundle");
        fs::write(&file, b"bundle-bytes").await.unwrap();

        let (path_tx, path_rx) = flume::unbounded::<PathBuf>();
        let cancel = CancellationToken::new();
        let task = tokio::spawn(watcher_task(
            dir.path().to_string_lossy().into_owned(),
            path_tx,
            cancel.clone(),
        ));

        // the timeout only bounds a regression
        let offered = tokio::time::timeout(Duration::from_secs(10), path_rx.recv_async())
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
        fs::write(&file, b"bundle-bytes").await.unwrap();

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
            CancellationToken::new(),
        )
        .await;

        assert!(
            fs::try_exists(dir.path().join("refused").join("refused.bundle"))
                .await
                .unwrap(),
            "refused file must be quarantined under refused/"
        );
        assert!(
            !fs::try_exists(&file).await.unwrap(),
            "refused file must leave the outbox"
        );

        // Drive the real restart sweep over the post-quarantine outbox. A
        // sentinel file proves the sweep ran; the quarantined file must not
        // be offered alongside it.
        let sentinel = dir.path().join("sentinel.bundle");
        fs::write(&sentinel, b"bundle-bytes").await.unwrap();

        let (path_tx, path_rx) = flume::unbounded::<PathBuf>();
        let cancel = CancellationToken::new();
        let task = tokio::spawn(watcher_task(
            dir.path().to_string_lossy().into_owned(),
            path_tx,
            cancel.clone(),
        ));

        // the timeout only bounds a regression
        let offered = tokio::time::timeout(Duration::from_secs(10), path_rx.recv_async())
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
