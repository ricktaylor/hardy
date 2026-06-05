use core::time::Duration;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use flume::{Receiver, Sender};
use futures::{FutureExt, select_biased};
use hardy_async::{BoundedTaskPool, CancellationToken, TaskPool};
use hardy_bpa::services::ApplicationSink;
use hardy_bpv7::eid::Eid;
use notify::event::{AccessKind, AccessMode, ModifyKind, RenameMode};
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use tracing::{debug, error, info, warn};

use crate::Error;

pub fn run(
    tasks: &TaskPool,
    sink: Arc<dyn ApplicationSink>,
    outbox: PathBuf,
    errors_dir: PathBuf,
    destination: Eid,
    lifetime: Duration,
) -> Result<(), Error> {
    let (event_tx, event_rx) = flume::unbounded();
    let startup_tx = event_tx.clone();

    // inotify-only: we rely on CLOSE_WRITE and MOVED_TO events which
    // PollWatcher cannot deliver. This limits us to native Linux.
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
        emit_existing_files(&outbox, &errors_dir, startup_tx).await;
        process_events(
            sink,
            event_rx,
            errors_dir,
            destination,
            lifetime,
            cancel_token,
        )
        .await;
    });

    Ok(())
}

async fn process_events(
    sink: Arc<dyn ApplicationSink>,
    event_rx: Receiver<Event>,
    errors_dir: PathBuf,
    destination: Eid,
    lifetime: Duration,
    cancel: CancellationToken,
) {
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
            let errors_dir = errors_dir.clone();
            let task_cancel = cancel.clone();

            select_biased! {
                _ = hardy_async::spawn!(senders, "outbox_send", async move {
                    process_file(path, sink, destination, lifetime, &errors_dir, task_cancel).await;
                }).fuse() => {}
                _ = cancel.cancelled().fuse() => break 'outer,
            }
        }
    }

    senders.shutdown().await;
    info!("Stopped watching outbox");
}

async fn emit_existing_files(outbox: &Path, errors_dir: &Path, tx: Sender<Event>) {
    let entries = match tokio::fs::read_dir(outbox).await {
        Ok(entries) => entries,
        Err(e) => {
            error!("Failed to scan outbox '{}': {e}", outbox.display());
            return;
        }
    };
    let mut recovered = 0;
    let mut existing = 0;
    let mut entries = entries;

    while let Ok(Some(entry)) = entries.next_entry().await {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }

        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");

        if name.ends_with(".processing") {
            let original_name = name.strip_suffix(".processing").unwrap();
            let original = path.with_file_name(original_name);
            if original.exists() {
                warn!(
                    "Cannot recover '{}': original file already exists, moving to errors",
                    path.display()
                );
                move_to_errors(&path, original_name.as_ref(), errors_dir).await;
                continue;
            }
            if let Err(e) = tokio::fs::rename(&path, &original).await {
                error!("Failed to recover '{}': {e}", path.display());
                continue;
            }
            recovered += 1;
        } else if is_processable(&path) {
            existing += 1;
            if tx
                .send(Event {
                    kind: EventKind::Access(AccessKind::Close(AccessMode::Write)),
                    paths: vec![path],
                    ..Event::default()
                })
                .is_err()
            {
                error!("Failed to queue existing file event");
                break;
            }
        }
    }

    if recovered > 0 || existing > 0 {
        info!("Startup: recovered {recovered} orphaned, queued {existing} existing file(s)");
    }
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

async fn move_to_errors(from: &Path, name: &OsStr, errors_dir: &Path) {
    let mut dest = errors_dir.join(name);
    let mut counter = 1u32;
    while dest.exists() {
        let mut suffixed = name.to_os_string();
        suffixed.push(format!(".{counter}"));
        dest = errors_dir.join(suffixed);
        counter += 1;
    }
    if let Err(e) = tokio::fs::rename(from, &dest).await {
        error!("Failed to move '{}' to errors: {e}", from.display());
    }
}

async fn process_file(
    path: PathBuf,
    sink: Arc<dyn ApplicationSink>,
    destination: Eid,
    lifetime: Duration,
    errors_dir: &Path,
    cancel: CancellationToken,
) {
    let name = path.file_name().unwrap_or_default().to_os_string();
    let mut processing_name = name.clone();
    processing_name.push(".processing");
    let processing_path = path.with_file_name(&processing_name);
    if let Err(e) = tokio::fs::rename(&path, &processing_path).await {
        if e.kind() == std::io::ErrorKind::NotFound {
            debug!("Skipping '{}': already claimed", path.display());
        } else {
            error!("Failed to claim '{}': {e}", path.display());
        }
        return;
    }

    let payload = match tokio::fs::read(&processing_path).await {
        Ok(payload) => payload,
        Err(e) => {
            error!("Failed to read '{}': {e}", processing_path.display());
            move_to_errors(&processing_path, &name, errors_dir).await;
            return;
        }
    };

    if payload.is_empty() {
        debug!("Discarding empty file '{}'", path.display());
        if let Err(e) = tokio::fs::remove_file(&processing_path).await {
            warn!(
                "Failed to remove empty file '{}': {e}",
                processing_path.display()
            );
        }
        return;
    }

    debug!(dest = %destination, bytes = payload.len(), "Sending payload from '{}'", path.display());
    let result = select_biased! {
        result = sink.send(destination, payload.into(), lifetime, None).fuse() => result,
        _ = cancel.cancelled().fuse() => {
            warn!("Cancelled sending '{}', restoring for next startup", path.display());
            if let Err(e) = tokio::fs::rename(&processing_path, &path).await {
                error!("Failed to restore '{}': {e}", path.display());
            }
            return;
        }
    };

    match result {
        Ok(id) => {
            debug!("Sent bundle {id}");
            if let Err(e) = tokio::fs::remove_file(&processing_path).await {
                warn!("Failed to remove '{}': {e}", processing_path.display());
            }
        }
        Err(e) => {
            error!("Failed to send bundle from '{}': {e}", path.display());
            move_to_errors(&processing_path, &name, errors_dir).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use notify::event::CreateKind;
    use std::fs;

    const ERRORS_DIR: &str = "errors";

    #[test]
    fn close_write_is_ready() {
        assert!(is_file_ready(&EventKind::Access(AccessKind::Close(
            AccessMode::Write
        ))));
    }

    #[test]
    fn moved_to_is_ready() {
        assert!(is_file_ready(&EventKind::Modify(ModifyKind::Name(
            RenameMode::To
        ))));
    }

    #[test]
    fn create_is_not_ready() {
        assert!(!is_file_ready(&EventKind::Create(CreateKind::File)));
    }

    #[test]
    fn close_read_is_not_ready() {
        assert!(!is_file_ready(&EventKind::Access(AccessKind::Close(
            AccessMode::Read
        ))));
    }

    #[test]
    fn regular_file_is_processable() {
        assert!(is_processable(Path::new("/outbox/payload.bin")));
    }

    #[test]
    fn dotfile_is_not_processable() {
        assert!(!is_processable(Path::new("/outbox/.tmp_file")));
    }

    #[test]
    fn processing_file_is_not_processable() {
        assert!(!is_processable(Path::new("/outbox/payload.bin.processing")));
    }

    #[tokio::test]
    async fn emit_existing_recovers_orphaned_files() {
        let dir = tempfile::tempdir().unwrap();
        let outbox = dir.path();
        fs::create_dir_all(outbox.join(ERRORS_DIR)).unwrap();

        fs::write(outbox.join("test.bin.processing"), "orphaned").unwrap();

        let (tx, rx) = flume::unbounded();
        emit_existing_files(outbox, &outbox.join(ERRORS_DIR), tx).await;

        assert!(outbox.join("test.bin").exists());
        assert!(!outbox.join("test.bin.processing").exists());
        drop(rx);
    }

    #[tokio::test]
    async fn emit_existing_queues_regular_files() {
        let dir = tempfile::tempdir().unwrap();
        let outbox = dir.path();
        fs::create_dir_all(outbox.join(ERRORS_DIR)).unwrap();

        fs::write(outbox.join("a.bin"), "payload_a").unwrap();
        fs::write(outbox.join("b.bin"), "payload_b").unwrap();
        fs::write(outbox.join(".hidden"), "ignored").unwrap();

        let (tx, rx) = flume::unbounded();
        emit_existing_files(outbox, &outbox.join(ERRORS_DIR), tx).await;

        let mut events: Vec<_> = rx.drain().collect();
        assert_eq!(events.len(), 2);

        let mut paths: Vec<_> = events
            .drain(..)
            .flat_map(|e| e.paths)
            .map(|p| p.file_name().unwrap().to_string_lossy().to_string())
            .collect();
        paths.sort();
        assert_eq!(paths, vec!["a.bin", "b.bin"]);
    }

    #[tokio::test]
    async fn emit_existing_handles_collision() {
        let dir = tempfile::tempdir().unwrap();
        let outbox = dir.path();
        fs::create_dir_all(outbox.join(ERRORS_DIR)).unwrap();

        fs::write(outbox.join("test.bin"), "original").unwrap();
        fs::write(outbox.join("test.bin.processing"), "orphaned").unwrap();

        let (tx, _rx) = flume::unbounded();
        emit_existing_files(outbox, &outbox.join(ERRORS_DIR), tx).await;

        assert!(outbox.join("test.bin").exists());
        assert!(!outbox.join("test.bin.processing").exists());
        assert!(outbox.join(ERRORS_DIR).join("test.bin").exists());
    }

    #[tokio::test]
    async fn emit_existing_skips_directories() {
        let dir = tempfile::tempdir().unwrap();
        let outbox = dir.path();
        fs::create_dir_all(outbox.join(ERRORS_DIR)).unwrap();
        fs::create_dir_all(outbox.join("subdir")).unwrap();
        fs::write(outbox.join("real.bin"), "data").unwrap();

        let (tx, rx) = flume::unbounded();
        emit_existing_files(outbox, &outbox.join(ERRORS_DIR), tx).await;

        let events: Vec<_> = rx.drain().collect();
        assert_eq!(events.len(), 1);
    }

    #[tokio::test]
    async fn emit_existing_skips_dotfiles() {
        let dir = tempfile::tempdir().unwrap();
        let outbox = dir.path();
        fs::create_dir_all(outbox.join(ERRORS_DIR)).unwrap();

        fs::write(outbox.join(".hidden"), "hidden").unwrap();
        fs::write(outbox.join("visible.bin"), "visible").unwrap();

        let (tx, rx) = flume::unbounded();
        emit_existing_files(outbox, &outbox.join(ERRORS_DIR), tx).await;

        let events: Vec<_> = rx.drain().collect();
        assert_eq!(events.len(), 1);
        assert!(events[0].paths[0].ends_with("visible.bin"));
    }

    #[tokio::test]
    async fn emit_existing_empty_dir() {
        let dir = tempfile::tempdir().unwrap();
        let outbox = dir.path();
        fs::create_dir_all(outbox.join(ERRORS_DIR)).unwrap();

        let (tx, rx) = flume::unbounded();
        emit_existing_files(outbox, &outbox.join(ERRORS_DIR), tx).await;

        let events: Vec<_> = rx.drain().collect();
        assert!(events.is_empty());
    }

    #[test]
    fn is_processable_no_extension() {
        assert!(is_processable(Path::new("/outbox/noext")));
    }

    #[test]
    fn is_processable_multiple_dots() {
        assert!(is_processable(Path::new("/outbox/file.tar.gz")));
    }

    #[test]
    fn is_processable_just_processing_extension() {
        assert!(!is_processable(Path::new("/outbox/file.processing")));
    }
}
