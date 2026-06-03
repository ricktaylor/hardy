use core::time::Duration;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use hardy_async::sync::spin::Once;
use hardy_async::{TaskPool, async_trait};
use hardy_bpa::Bytes;
use hardy_bpa::services::{Application, ApplicationSink, StatusNotify};
use hardy_bpv7::bundle::Id;
use hardy_bpv7::eid::Eid;
use hardy_bpv7::status_report::ReasonCode;
use time::OffsetDateTime;
use tracing::{error, info, warn};

mod error;
mod inbox;
mod outbox;

pub use error::Error;

const DEFAULT_LIFETIME: Duration = Duration::from_secs(86400);

pub(crate) fn ensure_dir(path: &Path) -> Result<(), Error> {
    std::fs::create_dir_all(path).map_err(|e| Error::CreateDir {
        path: path.display().to_string(),
        source: e,
    })?;
    Ok(())
}

pub struct FileService {
    destination: Eid,
    lifetime: Duration,
    outbox: Option<PathBuf>,
    inbox: Option<PathBuf>,
    sink: Once<Arc<dyn ApplicationSink>>,
    tasks: TaskPool,
}

impl FileService {
    pub fn new(
        destination: Eid,
        lifetime: Option<Duration>,
        outbox: Option<PathBuf>,
        inbox: Option<PathBuf>,
    ) -> Result<Self, Error> {
        let lifetime = lifetime.unwrap_or(DEFAULT_LIFETIME);

        if let Some(path) = &outbox {
            ensure_dir(path)?;
        }
        if let Some(path) = &inbox {
            ensure_dir(path)?;
        }

        Ok(Self {
            destination,
            lifetime,
            outbox,
            inbox,
            sink: Once::new(),
            tasks: TaskPool::new(),
        })
    }

    pub async fn unregister(&self) {
        self.tasks.shutdown().await;
        if let Some(sink) = self.sink.get() {
            sink.unregister().await;
        }
    }
}

#[async_trait]
impl Application for FileService {
    async fn on_register(&self, source: &Eid, sink: Box<dyn ApplicationSink>) {
        if self.sink.get().is_some() {
            error!("on_register called twice, ignoring");
            return;
        }

        info!("File service registered at {source}");

        let sink: &Arc<dyn ApplicationSink> = self.sink.call_once(|| Arc::from(sink));

        if let Some(outbox) = &self.outbox {
            if let Err(e) = outbox::run(
                &self.tasks,
                sink.clone(),
                outbox.clone(),
                self.destination.clone(),
                self.lifetime,
            ) {
                error!("Failed to start outbox watcher: {e}");
            }
        }
    }

    async fn on_unregister(&self) {
        self.tasks.shutdown().await;
    }

    async fn on_receive(
        &self,
        source: Eid,
        _expiry: OffsetDateTime,
        _ack_requested: bool,
        payload: Bytes,
    ) {
        if let Some(inbox) = &self.inbox {
            inbox::write_to_dir(inbox, &payload, &source).await;
        } else {
            warn!("Received payload from {source} but no inbox configured");
        }
    }

    async fn on_status_notify(
        &self,
        _bundle_id: &Id,
        _from: &Eid,
        _kind: StatusNotify,
        _reason: ReasonCode,
        _timestamp: Option<OffsetDateTime>,
    ) {
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hardy_async::sync::spin::Mutex;
    use std::str::FromStr;

    struct MockSink {
        sent: Mutex<Vec<(Eid, Vec<u8>)>>,
    }

    impl MockSink {
        fn new() -> Self {
            Self {
                sent: Mutex::new(Vec::new()),
            }
        }

        fn sent(&self) -> Vec<(Eid, Vec<u8>)> {
            self.sent.lock().clone()
        }
    }

    #[async_trait]
    impl ApplicationSink for MockSink {
        async fn unregister(&self) {}

        async fn send(
            &self,
            destination: Eid,
            data: Bytes,
            _lifetime: Duration,
            _options: Option<hardy_bpa::services::SendOptions>,
        ) -> hardy_bpa::services::Result<Id> {
            self.sent.lock().push((destination, data.to_vec()));
            Ok(Id {
                source: Eid::from_str("ipn:1.0").unwrap(),
                timestamp: hardy_bpv7::creation_timestamp::CreationTimestamp::default(),
                fragment_info: None,
            })
        }

        async fn cancel(&self, _bundle_id: &Id) -> hardy_bpa::services::Result<bool> {
            Ok(false)
        }
    }

    struct ArcSink(Arc<MockSink>);

    #[async_trait]
    impl ApplicationSink for ArcSink {
        async fn unregister(&self) {
            self.0.unregister().await;
        }

        async fn send(
            &self,
            destination: Eid,
            data: Bytes,
            lifetime: Duration,
            options: Option<hardy_bpa::services::SendOptions>,
        ) -> hardy_bpa::services::Result<Id> {
            self.0.send(destination, data, lifetime, options).await
        }

        async fn cancel(&self, bundle_id: &Id) -> hardy_bpa::services::Result<bool> {
            self.0.cancel(bundle_id).await
        }
    }

    struct FailingSink;

    #[async_trait]
    impl ApplicationSink for FailingSink {
        async fn unregister(&self) {}

        async fn send(
            &self,
            _destination: Eid,
            _data: Bytes,
            _lifetime: Duration,
            _options: Option<hardy_bpa::services::SendOptions>,
        ) -> hardy_bpa::services::Result<Id> {
            Err(hardy_bpa::services::Error::Internal(
                "simulated failure".into(),
            ))
        }

        async fn cancel(&self, _bundle_id: &Id) -> hardy_bpa::services::Result<bool> {
            Ok(false)
        }
    }

    fn mock_sink() -> (Arc<MockSink>, Box<dyn ApplicationSink>) {
        let mock = Arc::new(MockSink::new());
        let sink = Box::new(ArcSink(mock.clone()));
        (mock, sink)
    }

    fn test_eid() -> Eid {
        Eid::from_str("ipn:1.42").unwrap()
    }

    #[tokio::test]
    async fn outbox_sends_file_as_bundle() {
        let dir = tempfile::tempdir().unwrap();
        let outbox = dir.path().join("outbox");

        let service = FileService::new(
            test_eid(),
            Some(Duration::from_secs(60)),
            Some(outbox.clone()),
            None,
        )
        .unwrap();

        let mock = Arc::new(MockSink::new());
        service
            .on_register(
                &Eid::from_str("ipn:1.42").unwrap(),
                Box::new(ArcSink(mock.clone())),
            )
            .await;

        tokio::time::sleep(Duration::from_millis(100)).await;
        std::fs::write(outbox.join("test.bin"), "hello bundle").unwrap();
        tokio::time::sleep(Duration::from_secs(2)).await;

        let sent = mock.sent();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].0, test_eid());
        assert_eq!(sent[0].1, b"hello bundle");
        assert!(!outbox.join("test.bin").exists());

        service.unregister().await;
    }

    #[tokio::test]
    async fn outbox_recovers_processing_files_on_startup() {
        let dir = tempfile::tempdir().unwrap();
        let outbox = dir.path().join("outbox");
        std::fs::create_dir_all(&outbox).unwrap();
        std::fs::create_dir_all(outbox.join("errors")).unwrap();
        std::fs::write(outbox.join("orphan.bin.processing"), "recovered").unwrap();

        let service = FileService::new(
            test_eid(),
            Some(Duration::from_secs(60)),
            Some(outbox.clone()),
            None,
        )
        .unwrap();

        let mock = Arc::new(MockSink::new());
        service
            .on_register(
                &Eid::from_str("ipn:1.42").unwrap(),
                Box::new(ArcSink(mock.clone())),
            )
            .await;

        tokio::time::sleep(Duration::from_secs(2)).await;

        let sent = mock.sent();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].1, b"recovered");

        service.unregister().await;
    }

    #[tokio::test]
    async fn inbox_writes_received_payload() {
        let dir = tempfile::tempdir().unwrap();
        let inbox = dir.path().join("inbox");

        let service = FileService::new(
            test_eid(),
            Some(Duration::from_secs(60)),
            None,
            Some(inbox.clone()),
        )
        .unwrap();

        let source = Eid::from_str("ipn:3.1.7").unwrap();
        service
            .on_receive(
                source,
                OffsetDateTime::now_utc(),
                false,
                Bytes::from_static(b"incoming payload"),
            )
            .await;

        let entries: Vec<_> = std::fs::read_dir(&inbox).unwrap().flatten().collect();
        assert_eq!(entries.len(), 1);
        assert_eq!(
            std::fs::read(entries[0].path()).unwrap(),
            b"incoming payload"
        );
    }

    #[tokio::test]
    async fn outbox_sends_multiple_files_concurrently() {
        let dir = tempfile::tempdir().unwrap();
        let outbox = dir.path().join("outbox");

        let service = FileService::new(
            test_eid(),
            Some(Duration::from_secs(60)),
            Some(outbox.clone()),
            None,
        )
        .unwrap();

        let (mock, sink) = mock_sink();
        service
            .on_register(&Eid::from_str("ipn:1.42").unwrap(), sink)
            .await;

        tokio::time::sleep(Duration::from_millis(100)).await;
        for i in 0..10 {
            std::fs::write(outbox.join(format!("file_{i}.bin")), format!("payload_{i}")).unwrap();
        }
        tokio::time::sleep(Duration::from_secs(3)).await;

        let sent = mock.sent();
        assert_eq!(sent.len(), 10);

        let remaining: Vec<_> = std::fs::read_dir(&outbox)
            .unwrap()
            .flatten()
            .filter(|e| e.file_name() != "errors")
            .collect();
        assert!(remaining.is_empty(), "files should be deleted after send");

        service.unregister().await;
    }

    #[tokio::test]
    async fn outbox_ignores_dotfiles() {
        let dir = tempfile::tempdir().unwrap();
        let outbox = dir.path().join("outbox");

        let service = FileService::new(
            test_eid(),
            Some(Duration::from_secs(60)),
            Some(outbox.clone()),
            None,
        )
        .unwrap();

        let (mock, sink) = mock_sink();
        service
            .on_register(&Eid::from_str("ipn:1.42").unwrap(), sink)
            .await;

        tokio::time::sleep(Duration::from_millis(100)).await;
        std::fs::write(outbox.join(".hidden_file"), "should be ignored").unwrap();
        std::fs::write(outbox.join("visible_file"), "should be sent").unwrap();
        tokio::time::sleep(Duration::from_secs(2)).await;

        let sent = mock.sent();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].1, b"should be sent");

        service.unregister().await;
    }

    #[tokio::test]
    async fn outbox_discards_empty_files() {
        let dir = tempfile::tempdir().unwrap();
        let outbox = dir.path().join("outbox");

        let service = FileService::new(
            test_eid(),
            Some(Duration::from_secs(60)),
            Some(outbox.clone()),
            None,
        )
        .unwrap();

        let (mock, sink) = mock_sink();
        service
            .on_register(&Eid::from_str("ipn:1.42").unwrap(), sink)
            .await;

        tokio::time::sleep(Duration::from_millis(100)).await;
        std::fs::write(outbox.join("empty.bin"), "").unwrap();
        tokio::time::sleep(Duration::from_secs(2)).await;

        assert!(mock.sent().is_empty());
        assert!(!outbox.join("empty.bin").exists());
        assert!(!outbox.join("empty.bin.processing").exists());

        service.unregister().await;
    }

    #[tokio::test]
    async fn outbox_moves_failed_send_to_errors() {
        let dir = tempfile::tempdir().unwrap();
        let outbox = dir.path().join("outbox");

        let service = FileService::new(
            test_eid(),
            Some(Duration::from_secs(60)),
            Some(outbox.clone()),
            None,
        )
        .unwrap();

        service
            .on_register(&Eid::from_str("ipn:1.42").unwrap(), Box::new(FailingSink))
            .await;

        tokio::time::sleep(Duration::from_millis(100)).await;
        std::fs::write(outbox.join("will_fail.bin"), "doomed payload").unwrap();
        tokio::time::sleep(Duration::from_secs(2)).await;

        assert!(!outbox.join("will_fail.bin").exists());
        assert!(!outbox.join("will_fail.bin.processing").exists());
        assert!(outbox.join("errors").join("will_fail.bin").exists());
        assert_eq!(
            std::fs::read(outbox.join("errors").join("will_fail.bin")).unwrap(),
            b"doomed payload"
        );

        service.unregister().await;
    }

    #[tokio::test]
    async fn outbox_handles_mv_into_directory() {
        let dir = tempfile::tempdir().unwrap();
        let outbox = dir.path().join("outbox");
        let staging = dir.path().join("staging");
        std::fs::create_dir_all(&staging).unwrap();

        let service = FileService::new(
            test_eid(),
            Some(Duration::from_secs(60)),
            Some(outbox.clone()),
            None,
        )
        .unwrap();

        let (mock, sink) = mock_sink();
        service
            .on_register(&Eid::from_str("ipn:1.42").unwrap(), sink)
            .await;

        tokio::time::sleep(Duration::from_millis(100)).await;
        std::fs::write(staging.join("moved.bin"), "via rename").unwrap();
        std::fs::rename(staging.join("moved.bin"), outbox.join("moved.bin")).unwrap();
        tokio::time::sleep(Duration::from_secs(2)).await;

        let sent = mock.sent();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].1, b"via rename");

        service.unregister().await;
    }

    #[tokio::test]
    async fn outbox_processes_existing_files_on_startup() {
        let dir = tempfile::tempdir().unwrap();
        let outbox = dir.path().join("outbox");
        std::fs::create_dir_all(&outbox).unwrap();
        std::fs::create_dir_all(outbox.join("errors")).unwrap();

        std::fs::write(outbox.join("preexisting_a.bin"), "already here").unwrap();
        std::fs::write(outbox.join("preexisting_b.bin"), "also here").unwrap();

        let service = FileService::new(
            test_eid(),
            Some(Duration::from_secs(60)),
            Some(outbox.clone()),
            None,
        )
        .unwrap();

        let (mock, sink) = mock_sink();
        service
            .on_register(&Eid::from_str("ipn:1.42").unwrap(), sink)
            .await;

        tokio::time::sleep(Duration::from_secs(2)).await;

        let sent = mock.sent();
        assert_eq!(sent.len(), 2);

        service.unregister().await;
    }

    #[tokio::test]
    async fn inbox_handles_multiple_concurrent_receives() {
        let dir = tempfile::tempdir().unwrap();
        let inbox = dir.path().join("inbox");

        std::fs::create_dir_all(&inbox).unwrap();

        let source = Eid::from_str("ipn:3.1.7").unwrap();
        let mut handles = Vec::new();
        for i in 0..20 {
            let source = source.clone();
            let inbox = inbox.clone();
            let payload = Bytes::from(format!("payload_{i}"));
            handles.push(tokio::spawn(async move {
                inbox::write_to_dir(&inbox, &payload, &source).await;
            }));
        }
        for h in handles {
            h.await.unwrap();
        }

        let entries: Vec<_> = std::fs::read_dir(&inbox).unwrap().flatten().collect();
        assert_eq!(entries.len(), 20);
    }

    #[tokio::test]
    async fn service_without_outbox_or_inbox() {
        let service =
            FileService::new(test_eid(), Some(Duration::from_secs(60)), None, None).unwrap();

        let (_mock, sink) = mock_sink();
        service
            .on_register(&Eid::from_str("ipn:1.42").unwrap(), sink)
            .await;

        service
            .on_receive(
                Eid::from_str("ipn:3.1.7").unwrap(),
                OffsetDateTime::now_utc(),
                false,
                Bytes::from_static(b"no inbox"),
            )
            .await;

        service.unregister().await;
    }

    #[tokio::test]
    async fn outbox_preserves_binary_payload() {
        let dir = tempfile::tempdir().unwrap();
        let outbox = dir.path().join("outbox");

        let service = FileService::new(
            test_eid(),
            Some(Duration::from_secs(60)),
            Some(outbox.clone()),
            None,
        )
        .unwrap();

        let (mock, sink) = mock_sink();
        service
            .on_register(&Eid::from_str("ipn:1.42").unwrap(), sink)
            .await;

        let binary_data: Vec<u8> = (0..=255).collect();
        tokio::time::sleep(Duration::from_millis(100)).await;
        std::fs::write(outbox.join("binary.dat"), &binary_data).unwrap();
        tokio::time::sleep(Duration::from_secs(2)).await;

        let sent = mock.sent();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].1, binary_data);

        service.unregister().await;
    }

    #[tokio::test]
    async fn outbox_atomic_write_pattern() {
        let dir = tempfile::tempdir().unwrap();
        let outbox = dir.path().join("outbox");

        let service = FileService::new(
            test_eid(),
            Some(Duration::from_secs(60)),
            Some(outbox.clone()),
            None,
        )
        .unwrap();

        let (mock, sink) = mock_sink();
        service
            .on_register(&Eid::from_str("ipn:1.42").unwrap(), sink)
            .await;

        tokio::time::sleep(Duration::from_millis(100)).await;
        std::fs::write(outbox.join(".tmp_payload"), "atomic content").unwrap();
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(mock.sent().is_empty(), "dotfile should not trigger send");

        std::fs::rename(outbox.join(".tmp_payload"), outbox.join("payload.bin")).unwrap();
        tokio::time::sleep(Duration::from_secs(2)).await;

        let sent = mock.sent();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].1, b"atomic content");

        service.unregister().await;
    }

    #[tokio::test]
    async fn outbox_file_deleted_before_claim() {
        let dir = tempfile::tempdir().unwrap();
        let outbox = dir.path().join("outbox");

        let service = FileService::new(
            test_eid(),
            Some(Duration::from_secs(60)),
            Some(outbox.clone()),
            None,
        )
        .unwrap();

        let (mock, sink) = mock_sink();
        service
            .on_register(&Eid::from_str("ipn:1.42").unwrap(), sink)
            .await;

        tokio::time::sleep(Duration::from_millis(100)).await;
        std::fs::write(outbox.join("ephemeral.bin"), "short lived").unwrap();
        std::fs::remove_file(outbox.join("ephemeral.bin")).unwrap();
        tokio::time::sleep(Duration::from_secs(2)).await;

        assert!(mock.sent().is_empty());

        service.unregister().await;
    }

    #[tokio::test]
    async fn outbox_recovers_multiple_processing_files() {
        let dir = tempfile::tempdir().unwrap();
        let outbox = dir.path().join("outbox");
        std::fs::create_dir_all(&outbox).unwrap();
        std::fs::create_dir_all(outbox.join("errors")).unwrap();

        std::fs::write(outbox.join("a.bin.processing"), "payload_a").unwrap();
        std::fs::write(outbox.join("b.bin.processing"), "payload_b").unwrap();
        std::fs::write(outbox.join("c.bin.processing"), "payload_c").unwrap();

        let service = FileService::new(
            test_eid(),
            Some(Duration::from_secs(60)),
            Some(outbox.clone()),
            None,
        )
        .unwrap();

        let (mock, sink) = mock_sink();
        service
            .on_register(&Eid::from_str("ipn:1.42").unwrap(), sink)
            .await;

        tokio::time::sleep(Duration::from_secs(2)).await;

        let sent = mock.sent();
        assert_eq!(sent.len(), 3);

        service.unregister().await;
    }

    #[tokio::test]
    async fn outbox_ignores_errors_dir_files() {
        let dir = tempfile::tempdir().unwrap();
        let outbox = dir.path().join("outbox");
        std::fs::create_dir_all(&outbox).unwrap();
        std::fs::create_dir_all(outbox.join("errors")).unwrap();

        std::fs::write(outbox.join("errors").join("old_failure.bin"), "should stay").unwrap();
        std::fs::write(outbox.join("real_file.bin"), "should send").unwrap();

        let service = FileService::new(
            test_eid(),
            Some(Duration::from_secs(60)),
            Some(outbox.clone()),
            None,
        )
        .unwrap();

        let (mock, sink) = mock_sink();
        service
            .on_register(&Eid::from_str("ipn:1.42").unwrap(), sink)
            .await;

        tokio::time::sleep(Duration::from_secs(2)).await;

        let sent = mock.sent();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].1, b"should send");
        assert!(outbox.join("errors").join("old_failure.bin").exists());

        service.unregister().await;
    }

    #[tokio::test]
    async fn outbox_ignores_subdirectories() {
        let dir = tempfile::tempdir().unwrap();
        let outbox = dir.path().join("outbox");

        let service = FileService::new(
            test_eid(),
            Some(Duration::from_secs(60)),
            Some(outbox.clone()),
            None,
        )
        .unwrap();

        let (mock, sink) = mock_sink();
        service
            .on_register(&Eid::from_str("ipn:1.42").unwrap(), sink)
            .await;

        tokio::time::sleep(Duration::from_millis(100)).await;
        std::fs::create_dir_all(outbox.join("subdir")).unwrap();
        std::fs::write(outbox.join("real.bin"), "payload").unwrap();
        tokio::time::sleep(Duration::from_secs(2)).await;

        let sent = mock.sent();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].1, b"payload");

        service.unregister().await;
    }

    #[tokio::test]
    async fn unregister_drains_inflight_sends() {
        let dir = tempfile::tempdir().unwrap();
        let outbox = dir.path().join("outbox");

        let service = FileService::new(
            test_eid(),
            Some(Duration::from_secs(60)),
            Some(outbox.clone()),
            None,
        )
        .unwrap();

        let (mock, sink) = mock_sink();
        service
            .on_register(&Eid::from_str("ipn:1.42").unwrap(), sink)
            .await;

        tokio::time::sleep(Duration::from_millis(100)).await;
        for i in 0..5 {
            std::fs::write(outbox.join(format!("drain_{i}.bin")), format!("data_{i}")).unwrap();
        }
        tokio::time::sleep(Duration::from_millis(500)).await;

        service.unregister().await;

        let remaining: Vec<_> = std::fs::read_dir(&outbox)
            .unwrap()
            .flatten()
            .filter(|e| {
                let name = e.file_name().to_string_lossy().to_string();
                name != "errors" && !name.ends_with(".processing")
            })
            .collect();
        assert!(
            remaining.is_empty(),
            "no regular files should remain after drain: {:?}",
            remaining.iter().map(|e| e.file_name()).collect::<Vec<_>>()
        );

        assert!(mock.sent().len() <= 5);

        let processing: Vec<_> = std::fs::read_dir(&outbox)
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().ends_with(".processing"))
            .collect();
        assert!(
            processing.is_empty(),
            "no .processing files should remain: {:?}",
            processing.iter().map(|e| e.file_name()).collect::<Vec<_>>()
        );
    }

    #[tokio::test]
    async fn inbox_preserves_binary_content() {
        let dir = tempfile::tempdir().unwrap();
        let inbox = dir.path().join("inbox");

        let service = FileService::new(
            test_eid(),
            Some(Duration::from_secs(60)),
            None,
            Some(inbox.clone()),
        )
        .unwrap();

        let binary_data: Vec<u8> = (0..=255).cycle().take(1024).collect();
        let source = Eid::from_str("ipn:3.1.7").unwrap();
        service
            .on_receive(
                source,
                OffsetDateTime::now_utc(),
                false,
                Bytes::from(binary_data.clone()),
            )
            .await;

        let entries: Vec<_> = std::fs::read_dir(&inbox).unwrap().flatten().collect();
        assert_eq!(entries.len(), 1);
        assert_eq!(std::fs::read(entries[0].path()).unwrap(), binary_data);
    }

    // --- Startup / construction ---

    #[test]
    fn new_uses_default_lifetime() {
        let dir = tempfile::tempdir().unwrap();
        let service =
            FileService::new(test_eid(), None, Some(dir.path().join("outbox")), None).unwrap();
        assert_eq!(service.lifetime, DEFAULT_LIFETIME);
    }

    #[test]
    fn new_creates_outbox_and_inbox_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let outbox = dir.path().join("deep/nested/outbox");
        let inbox = dir.path().join("deep/nested/inbox");

        FileService::new(test_eid(), None, Some(outbox.clone()), Some(inbox.clone())).unwrap();

        assert!(outbox.is_dir());
        assert!(inbox.is_dir());
    }

    // --- Outbox: error directory behavior ---

    #[tokio::test]
    async fn errors_dir_preserves_all_failures() {
        let dir = tempfile::tempdir().unwrap();
        let outbox = dir.path().join("outbox");

        let service = FileService::new(
            test_eid(),
            Some(Duration::from_secs(60)),
            Some(outbox.clone()),
            None,
        )
        .unwrap();

        service
            .on_register(&Eid::from_str("ipn:1.42").unwrap(), Box::new(FailingSink))
            .await;

        tokio::time::sleep(Duration::from_millis(100)).await;
        std::fs::write(outbox.join("collide.bin"), "first attempt").unwrap();
        tokio::time::sleep(Duration::from_secs(2)).await;

        assert!(outbox.join("errors/collide.bin").exists());
        assert_eq!(
            std::fs::read_to_string(outbox.join("errors/collide.bin")).unwrap(),
            "first attempt"
        );

        std::fs::write(outbox.join("collide.bin"), "second attempt").unwrap();
        tokio::time::sleep(Duration::from_secs(2)).await;

        assert!(outbox.join("errors/collide.bin.1").exists());
        assert_eq!(
            std::fs::read_to_string(outbox.join("errors/collide.bin.1")).unwrap(),
            "second attempt"
        );

        service.unregister().await;
    }

    // --- Outbox: payload correctness ---

    #[tokio::test]
    async fn outbox_sends_correct_destination_and_lifetime() {
        let dir = tempfile::tempdir().unwrap();
        let outbox = dir.path().join("outbox");
        let dest = Eid::from_str("ipn:99.1.7").unwrap();

        let service = FileService::new(
            dest.clone(),
            Some(Duration::from_secs(3600)),
            Some(outbox.clone()),
            None,
        )
        .unwrap();

        let (mock, sink) = mock_sink();
        service
            .on_register(&Eid::from_str("ipn:1.42").unwrap(), sink)
            .await;

        tokio::time::sleep(Duration::from_millis(100)).await;
        std::fs::write(outbox.join("dest_test.bin"), "check dest").unwrap();
        tokio::time::sleep(Duration::from_secs(2)).await;

        let sent = mock.sent();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].0, dest);

        service.unregister().await;
    }

    // --- Outbox: large payload ---

    #[tokio::test]
    async fn outbox_handles_large_payload() {
        let dir = tempfile::tempdir().unwrap();
        let outbox = dir.path().join("outbox");

        let service = FileService::new(
            test_eid(),
            Some(Duration::from_secs(60)),
            Some(outbox.clone()),
            None,
        )
        .unwrap();

        let (mock, sink) = mock_sink();
        service
            .on_register(&Eid::from_str("ipn:1.42").unwrap(), sink)
            .await;

        let large_payload: Vec<u8> = vec![0xAB; 1024 * 1024]; // 1MB
        tokio::time::sleep(Duration::from_millis(100)).await;
        std::fs::write(outbox.join("large.bin"), &large_payload).unwrap();
        tokio::time::sleep(Duration::from_secs(3)).await;

        let sent = mock.sent();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].1.len(), 1024 * 1024);
        assert_eq!(sent[0].1, large_payload);

        service.unregister().await;
    }

    // --- Outbox: rapid writes ---

    #[tokio::test]
    async fn outbox_handles_rapid_sequential_writes() {
        let dir = tempfile::tempdir().unwrap();
        let outbox = dir.path().join("outbox");

        let service = FileService::new(
            test_eid(),
            Some(Duration::from_secs(60)),
            Some(outbox.clone()),
            None,
        )
        .unwrap();

        let (mock, sink) = mock_sink();
        service
            .on_register(&Eid::from_str("ipn:1.42").unwrap(), sink)
            .await;

        tokio::time::sleep(Duration::from_millis(100)).await;
        for i in 0..50 {
            std::fs::write(outbox.join(format!("rapid_{i:03}.bin")), format!("p{i}")).unwrap();
        }
        tokio::time::sleep(Duration::from_secs(5)).await;

        let sent = mock.sent();
        assert_eq!(sent.len(), 50);

        let remaining: Vec<_> = std::fs::read_dir(&outbox)
            .unwrap()
            .flatten()
            .filter(|e| e.file_name() != "errors")
            .collect();
        assert!(remaining.is_empty());

        service.unregister().await;
    }

    // --- Outbox: file with no extension ---

    #[tokio::test]
    async fn outbox_handles_file_without_extension() {
        let dir = tempfile::tempdir().unwrap();
        let outbox = dir.path().join("outbox");

        let service = FileService::new(
            test_eid(),
            Some(Duration::from_secs(60)),
            Some(outbox.clone()),
            None,
        )
        .unwrap();

        let (mock, sink) = mock_sink();
        service
            .on_register(&Eid::from_str("ipn:1.42").unwrap(), sink)
            .await;

        tokio::time::sleep(Duration::from_millis(100)).await;
        std::fs::write(outbox.join("noext"), "no extension here").unwrap();
        tokio::time::sleep(Duration::from_secs(2)).await;

        let sent = mock.sent();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].1, b"no extension here");

        service.unregister().await;
    }

    // --- Inbox: multiple sources ---

    #[tokio::test]
    async fn inbox_differentiates_sources_in_filenames() {
        let dir = tempfile::tempdir().unwrap();
        let inbox = dir.path().join("inbox");

        let service = FileService::new(
            test_eid(),
            Some(Duration::from_secs(60)),
            None,
            Some(inbox.clone()),
        )
        .unwrap();

        let source_a = Eid::from_str("ipn:1.42").unwrap();
        let source_b = Eid::from_str("ipn:2.42").unwrap();

        service
            .on_receive(
                source_a,
                OffsetDateTime::now_utc(),
                false,
                Bytes::from_static(b"from_a"),
            )
            .await;
        service
            .on_receive(
                source_b,
                OffsetDateTime::now_utc(),
                false,
                Bytes::from_static(b"from_b"),
            )
            .await;

        let entries: Vec<_> = std::fs::read_dir(&inbox).unwrap().flatten().collect();
        assert_eq!(entries.len(), 2);

        let names: Vec<_> = entries
            .iter()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        let has_source_a = names.iter().any(|n| n.contains("ipn_1.42"));
        let has_source_b = names.iter().any(|n| n.contains("ipn_2.42"));
        assert!(has_source_a, "should contain source_a in filename");
        assert!(has_source_b, "should contain source_b in filename");
    }
}
