use std::path::Path;

use tracing::{error, info};

use crate::{parser, scheduler::SchedulerHandle};

// Brings the scheduler in line with the contact plan file after a watched
// change. A deleted file withdraws the file's contacts; a file that cannot be
// read or parsed leaves the current contacts in place.
pub async fn reload(path: &Path, source: &str, priority: u32, scheduler: &SchedulerHandle) {
    if !path.exists() {
        info!("Contact plan file removed, withdrawing all contacts");
        scheduler.withdraw_all(source).await;
        return;
    }

    info!("Reloading contact plan from '{}'", path.display());
    match parser::load_contacts(path).await {
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
            error!("Failed to reload contact plan, keeping the current contacts: {e}");
            metrics::counter!("tvr_file_reloads", "outcome" => "error").increment(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::PathBuf,
        sync::{Arc, Mutex},
    };

    use hardy_async::{TaskPool, async_trait};
    use hardy_bpa::routing::{Result, RouteAction, RoutingSink};
    use hardy_eid_patterns::EidPattern;
    use tempfile::TempDir;

    use super::*;
    use crate::scheduler;

    const SOURCE: &str = "file:test";
    const PRIORITY: u32 = 100;
    const PLAN: &str = "ipn:2.*.* via ipn:2.1.0\n";

    #[derive(Debug, PartialEq)]
    enum Call {
        Add(EidPattern, RouteAction, u32),
        Remove(EidPattern, RouteAction, u32),
    }

    // Records every route change the scheduler makes, in order.
    #[derive(Default)]
    struct RecordingSink {
        calls: Mutex<Vec<Call>>,
    }

    #[async_trait]
    impl RoutingSink for RecordingSink {
        async fn unregister(&self) {}

        async fn add_route(
            &self,
            pattern: EidPattern,
            action: RouteAction,
            priority: u32,
        ) -> Result<bool> {
            self.calls
                .lock()
                .unwrap()
                .push(Call::Add(pattern, action, priority));
            Ok(true)
        }

        async fn remove_route(
            &self,
            pattern: &EidPattern,
            action: &RouteAction,
            priority: u32,
        ) -> Result<bool> {
            self.calls.lock().unwrap().push(Call::Remove(
                pattern.clone(),
                action.clone(),
                priority,
            ));
            Ok(true)
        }
    }

    fn add() -> Call {
        Call::Add(
            "ipn:2.*.*".parse().unwrap(),
            RouteAction::Via("ipn:2.1.0".parse().unwrap()),
            PRIORITY,
        )
    }

    fn remove() -> Call {
        Call::Remove(
            "ipn:2.*.*".parse().unwrap(),
            RouteAction::Via("ipn:2.1.0".parse().unwrap()),
            PRIORITY,
        )
    }

    // A running scheduler over a recording sink, and a contact plan file
    // whose contact is already installed.
    struct Fixture {
        _dir: TempDir,
        path: PathBuf,
        sink: Arc<RecordingSink>,
        scheduler: SchedulerHandle,
        tasks: TaskPool,
    }

    impl Fixture {
        async fn installed() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("contact_plan");
            fs::write(&path, PLAN).unwrap();

            let sink = Arc::new(RecordingSink::default());
            let tasks = TaskPool::new();
            let (scheduler, rx) = scheduler::channel();
            scheduler::start(rx, sink.clone(), &tasks);

            let fixture = Self {
                _dir: dir,
                path,
                sink,
                scheduler,
                tasks,
            };
            fixture.reload().await;
            fixture
        }

        async fn reload(&self) {
            reload(&self.path, SOURCE, PRIORITY, &self.scheduler).await;
        }

        // Restores the original plan and reloads it. The scheduler answers
        // the replace only after every earlier command and its route changes,
        // so the recorded calls are complete once this returns.
        async fn restore(self) -> Vec<Call> {
            fs::write(&self.path, PLAN).unwrap();
            self.reload().await;
            self.tasks.shutdown().await;
            self.sink.calls.lock().unwrap().drain(..).collect()
        }
    }

    #[tokio::test]
    async fn a_reload_that_fails_to_parse_keeps_the_contacts() {
        let fixture = Fixture::installed().await;

        fs::write(&fixture.path, "ipn:2.*.* via Broken\n").unwrap();
        fixture.reload().await;

        assert_eq!(fixture.restore().await, vec![add()]);
    }

    #[tokio::test]
    async fn a_reload_that_fails_to_read_keeps_the_contacts() {
        let fixture = Fixture::installed().await;

        // Not UTF-8, so the read itself fails.
        fs::write(&fixture.path, [0xff, 0xfe]).unwrap();
        fixture.reload().await;

        assert_eq!(fixture.restore().await, vec![add()]);
    }

    #[tokio::test]
    async fn a_deleted_plan_withdraws_its_contacts() {
        let fixture = Fixture::installed().await;

        fs::remove_file(&fixture.path).unwrap();
        fixture.reload().await;

        assert_eq!(fixture.restore().await, vec![add(), remove(), add()]);
    }
}
