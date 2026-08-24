use std::{collections::HashMap, path::Path, time::Duration};

use hardy_bpa::cla::{Cla as _, ClaAddress};
use hardy_bpv7::eid::NodeId;
use hardy_file_cla::{Cla, Config};

// Regression bound for inotify-driven outcomes, not a synchronization
// primitive: the watcher debounces events for 1 second and the OS offers
// no synchronous hook, so tests wait on the observable outcome with a
// deadline far above the debounce window.
const REGRESSION_BOUND: Duration = Duration::from_secs(30);

// A Sink stub that forwards every dispatched bundle's bytes to a channel
// the test can await.
struct StubSink(flume::Sender<Vec<u8>>);

#[hardy_bpa::async_trait]
impl hardy_bpa::cla::Sink for StubSink {
    async fn unregister(&self) {}

    async fn dispatch(
        &self,
        _peer_node: Option<&NodeId>,
        _peer_addr: Option<&ClaAddress>,
        stream: &mut dyn hardy_bpa::stream::Receiver<hardy_bpa::cla::Segment>,
    ) -> hardy_bpa::cla::Result<()> {
        let mut buffer = Vec::new();
        loop {
            match stream
                .recv()
                .await
                .map_err(|_| hardy_bpa::cla::Error::StreamCancelled)?
            {
                hardy_bpa::cla::Segment::Next(bytes) => buffer.extend_from_slice(&bytes),
                hardy_bpa::cla::Segment::Final(bytes) => {
                    buffer.extend_from_slice(&bytes);
                    break;
                }
            }
        }
        self.0
            .send_async(buffer)
            .await
            .map_err(|_| hardy_bpa::cla::Error::Disconnected)
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
        _bundle_id: &hardy_bpv7::bundle::Id,
        _outcome: hardy_bpa::cla::TransferOutcome,
    ) -> hardy_bpa::cla::Result<()> {
        Ok(())
    }
}

// Registered CLA watching `outbox`, plus the channel of dispatched bundles.
async fn start_cla(outbox: &Path) -> (Cla, flume::Receiver<Vec<u8>>) {
    let cla = Cla::new(&Config {
        outbox: Some(outbox.to_path_buf()),
        peers: HashMap::new(),
    })
    .unwrap();

    let (tx, rx) = flume::unbounded();
    cla.on_register(Box::new(StubSink(tx)), &[]).await;
    (cla, rx)
}

async fn recv_dispatch(rx: &flume::Receiver<Vec<u8>>) -> Vec<u8> {
    tokio::time::timeout(REGRESSION_BOUND, rx.recv_async())
        .await
        .expect("bundle was not dispatched within the regression bound")
        .expect("dispatch channel closed")
}

// Poll until the file has been removed after dispatch; deletion follows
// dispatch with no synchronous hook, so this is deadline-bounded polling
// on the observable outcome.
async fn wait_removed(path: &Path) {
    let deadline = tokio::time::Instant::now() + REGRESSION_BOUND;
    while path.exists() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "dispatched file was not removed within the regression bound"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

// Files already queued in the outbox when the CLA starts are dispatched
// and removed: the removable-media use case means bundles routinely arrive
// while the CLA is not running.
#[tokio::test]
async fn test_outbox_pickup_preexisting_file() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("queued"), b"queued-bundle").unwrap();

    let (cla, rx) = start_cla(dir.path()).await;

    assert_eq!(recv_dispatch(&rx).await, b"queued-bundle");
    wait_removed(&dir.path().join("queued")).await;

    cla.on_unregister().await;
}

// A file created after the watcher starts is dispatched and removed. The
// marker file's dispatch proves the one-shot startup scan has finished, so
// the second file can only be picked up via a filesystem event.
#[tokio::test]
async fn test_outbox_pickup_created_file() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("marker"), b"marker-bundle").unwrap();
    let (cla, rx) = start_cla(dir.path()).await;
    assert_eq!(recv_dispatch(&rx).await, b"marker-bundle");

    std::fs::write(dir.path().join("created"), b"created-bundle").unwrap();

    assert_eq!(recv_dispatch(&rx).await, b"created-bundle");
    wait_removed(&dir.path().join("created")).await;

    cla.on_unregister().await;
}

// on_unregister stops the watcher and forwarder tasks; a hang here is a
// shutdown regression.
#[tokio::test]
async fn test_shutdown_stops_watcher() {
    let dir = tempfile::tempdir().unwrap();
    let (cla, rx) = start_cla(dir.path()).await;

    // Prove the watcher is live before shutting it down.
    std::fs::write(dir.path().join("live"), b"live-bundle").unwrap();
    assert_eq!(recv_dispatch(&rx).await, b"live-bundle");

    tokio::time::timeout(REGRESSION_BOUND, cla.on_unregister())
        .await
        .expect("shutdown did not complete within the regression bound");
}
