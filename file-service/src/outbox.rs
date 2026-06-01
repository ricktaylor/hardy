use core::time::Duration;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use hardy_async::{BoundedTaskPool, CancellationToken, TaskPool};
use hardy_bpa::services::ApplicationSink;
use hardy_bpv7::eid::Eid;
use notify::event::{AccessKind, AccessMode, ModifyKind, RenameMode};
use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use tracing::{debug, error, info, warn};

use crate::Error;

pub fn start(
    tasks: &TaskPool,
    sink: Arc<dyn ApplicationSink>,
    outbox: PathBuf,
    destination: Eid,
    lifetime: Duration,
) -> Result<(), Error> {
    let (event_tx, event_rx) = flume::unbounded();

    let mut watcher = RecommendedWatcher::new(
        move |result| match result {
            Ok(event) => {
                if event_tx.send(event).is_err() {
                    error!("Event channel closed");
                }
            }
            Err(e) => {
                error!("Watch error: {e}");
            }
        },
        notify::Config::default(),
    )
    .map_err(|e| Error::Watch {
        path: outbox.display().to_string(),
        source: e,
    })?;

    watcher
        .watch(&outbox, RecursiveMode::NonRecursive)
        .map_err(|e| Error::Watch {
            path: outbox.display().to_string(),
            source: e,
        })?;

    let cancel_token = tasks.cancel_token().clone();
    hardy_async::spawn!(tasks, "outbox_watcher", async move {
        let _watcher = watcher;
        run(sink, event_rx, outbox, destination, lifetime, cancel_token).await;
    });

    Ok(())
}

async fn run(
    sink: Arc<dyn ApplicationSink>,
    event_rx: flume::Receiver<notify::Event>,
    outbox: PathBuf,
    destination: Eid,
    lifetime: Duration,
    cancel: CancellationToken,
) {
    info!("Watching outbox '{}'", outbox.display());

    let senders = BoundedTaskPool::default();

    'outer: loop {
        let event = tokio::select! {
            result = event_rx.recv_async() => {
                match result {
                    Err(_) => break,
                    Ok(event) => event,
                }
            }
            _ = cancel.cancelled() => break,
        };

        if !is_file_ready(&event.kind) {
            continue;
        }

        for path in event.paths {
            if !is_processable(&path) {
                continue;
            }
            let sink = sink.clone();
            let destination = destination.clone();
            let spawn_fut = hardy_async::spawn!(senders, "outbox_send", async move {
                process_file(path, sink, destination, lifetime).await;
            });
            tokio::select! {
                _ = spawn_fut => {}
                _ = cancel.cancelled() => break 'outer,
            }
        }
    }

    senders.shutdown().await;
    info!("Stopped watching outbox '{}'", outbox.display());
}

fn is_file_ready(kind: &EventKind) -> bool {
    matches!(
        kind,
        EventKind::Access(AccessKind::Close(AccessMode::Write))
            | EventKind::Modify(ModifyKind::Name(RenameMode::To))
    )
}

fn is_processable(path: &Path) -> bool {
    let name = path.file_name().and_then(|name| name.to_str());
    let ext = path.extension().and_then(|ext| ext.to_str());
    name.is_some_and(|n| !n.starts_with('.')) && ext != Some("processing")
}

async fn process_file(
    path: PathBuf,
    sink: Arc<dyn ApplicationSink>,
    destination: Eid,
    lifetime: Duration,
) {
    let mut processing_name = path.file_name().unwrap_or_default().to_os_string();
    processing_name.push(".processing");
    let processing_path = path.with_file_name(processing_name);
    if let Err(e) = tokio::fs::rename(&path, &processing_path).await {
        debug!("Skipping '{}': {e}", path.display());
        return;
    }

    let payload = match tokio::fs::read(&processing_path).await {
        Ok(payload) => payload,
        Err(e) => {
            warn!("Failed to read '{}': {e}", processing_path.display());
            return;
        }
    };

    if payload.is_empty() {
        if let Err(e) = tokio::fs::remove_file(&processing_path).await {
            warn!(
                "Failed to remove empty file '{}': {e}",
                processing_path.display()
            );
        }
        return;
    }

    debug!(dest = %destination, bytes = payload.len(), "Sending payload from '{}'", path.display());
    match sink.send(destination, payload.into(), lifetime, None).await {
        Ok(id) => {
            debug!("Sent bundle {id}");
            if let Err(e) = tokio::fs::remove_file(&processing_path).await {
                warn!("Failed to remove '{}': {e}", processing_path.display());
            }
        }
        Err(e) => {
            warn!(
                "Failed to send bundle from '{}': {e}. Restoring file.",
                path.display()
            );
            if let Err(e) = tokio::fs::rename(&processing_path, &path).await {
                error!("Failed to restore '{}': {e}", path.display());
            }
        }
    }
}
