use crate::parser;
use crate::scheduler::SchedulerHandle;
use notify_debouncer_full::{
    DebouncedEvent, new_debouncer,
    notify::{
        EventKind, RecursiveMode,
        event::{CreateKind, RemoveKind},
    },
};
use std::path::PathBuf;
use tracing::{error, info};

// Start a file watcher that reloads the contact plan on changes.
//
// On file create/modify: re-parse → `replace_contacts` into scheduler.
// On file delete: withdraw all contacts from this source.
pub fn start(
    contact_plan: PathBuf,
    priority: u32,
    scheduler: SchedulerHandle,
    tasks: &hardy_async::TaskPool,
) {
    let watch_dir = contact_plan
        .parent()
        .expect("contact plan file must have a parent directory")
        .to_path_buf();
    let cancel = tasks.cancel_token().clone();

    hardy_async::spawn!(tasks, "contact_plan_watcher", async move {
        let (tx, rx) = flume::unbounded();
        let mut debouncer = match new_debouncer(
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
                Err(errors) => {
                    for e in errors {
                        error!("File watch error: {e}");
                    }
                }
            },
        ) {
            Ok(d) => d,
            Err(e) => {
                error!("Failed to create file watcher: {e}");
                return;
            }
        };

        if let Err(e) = debouncer.watch(&watch_dir, RecursiveMode::NonRecursive) {
            error!("Failed to watch '{}': {e}", watch_dir.display());
            return;
        }

        info!(
            "Watching contact plan file '{}' for changes",
            contact_plan.display()
        );

        let source = format!("file:{}", contact_plan.display());

        loop {
            tokio::select! {
                res = rx.recv_async() => match res {
                    Err(_) => break,
                    Ok(DebouncedEvent { event, .. }) => {
                        let dominated = matches!(
                            event.kind,
                            EventKind::Create(CreateKind::File)
                                | EventKind::Modify(_)
                                | EventKind::Remove(RemoveKind::File)
                        );
                        if dominated && event.paths.iter().any(|p| p == &contact_plan) {
                            reload(&contact_plan, &source, priority, &scheduler, &event.kind).await;
                        }
                    }
                },
                _ = cancel.cancelled() => break,
            }
        }
    });
}

async fn reload(
    path: &PathBuf,
    source: &str,
    priority: u32,
    scheduler: &SchedulerHandle,
    event_kind: &EventKind,
) {
    if matches!(event_kind, EventKind::Remove(_)) {
        info!("Contact plan file removed, withdrawing all contacts");
        scheduler.withdraw_all(source).await;
        return;
    }

    info!("Reloading contact plan from '{}'", path.display());
    match parser::load_contacts(path, true, true).await {
        Ok(contacts) => {
            if let Some(result) = scheduler.replace_contacts(source, contacts, priority).await {
                info!(
                    "Contact plan reloaded: {} added, {} removed, {} unchanged",
                    result.added, result.removed, result.unchanged
                );
            }
            metrics::counter!("tvr_file_reloads", "outcome" => "success").increment(1);
        }
        Err(e) => {
            error!("Failed to reload contact plan: {e}");
            metrics::counter!("tvr_file_reloads", "outcome" => "error").increment(1);
        }
    }
}
