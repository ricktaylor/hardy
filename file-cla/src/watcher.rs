use super::*;
use notify_debouncer_full::{
    DebouncedEvent, new_debouncer,
    notify::{EventKind, RecursiveMode, event::CreateKind},
};

impl Cla {
    pub async fn start_watcher(&self, sink: Arc<dyn hardy_bpa::cla::Sink>, outbox: String) {
        let (path_tx, path_rx) = flume::unbounded::<PathBuf>();

        let cancel_token = self.cancel_token.clone();
        let task = async move { watcher_task(outbox, path_tx, cancel_token).await };

        #[cfg(feature = "tracing")]
        let task = {
            let span = tracing::trace_span!(parent: None, "watcher_task");
            span.follows_from(tracing::Span::current());
            task.instrument(span)
        };

        self.task_tracker.spawn(task);

        let cancel_token = self.cancel_token.clone();
        let task = async move { forwarder_task(sink, path_rx, cancel_token).await };

        #[cfg(feature = "tracing")]
        let task = {
            let span = tracing::trace_span!(parent: None, "forwarder_task");
            span.follows_from(tracing::Span::current());
            task.instrument(span)
        };

        self.task_tracker.spawn(task);
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
    cancel_token: tokio_util::sync::CancellationToken,
) {
    loop {
        tokio::select! {
            res = rx.recv_async() => match res {
                Err(_) => break,
                Ok(path) => {
                    if let Ok(buffer) = tokio::fs::read(&path).await.inspect_err(|e| error!("Failed to read from '{}': {e}", path.display())) {
                        match sink.dispatch(buffer.into()).await {
                            Err(e) => warn!("Failed to dispatch bundle: {e}"),
                            Ok(_) => debug!("Dispatched '{}'",path.display()),
                        }
                    }

                    // TODO:  We could implement a "Sent Items" folder instead of deleting, but not sure...
                    tokio::fs::remove_file(&path).await.unwrap_or_else(|e| {
                        warn!("Failed to remove file '{}': {e}", path.display());
                    });
                }
            },
            _ = cancel_token.cancelled() => {
                break;
            }
        }
    }
}
