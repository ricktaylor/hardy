use std::{
    collections::HashMap,
    path::Path,
    sync::{Arc, Mutex},
};

use hardy_bpa::cla::{Cla as _, ClaAddress, ForwardBundleResult};
use hardy_bpv7::{bundle::FragmentInfo, eid::NodeId};
use hardy_file_cla::{Cla, Config};

// A Sink stub that records add_peer addresses; the forward path never
// dispatches inbound bundles.
struct StubSink(Arc<Mutex<Vec<ClaAddress>>>);

#[hardy_bpa::async_trait]
impl hardy_bpa::cla::Sink for StubSink {
    async fn unregister(&self) {}

    async fn dispatch(
        &self,
        _peer_node: Option<&NodeId>,
        _peer_addr: Option<&ClaAddress>,
        _stream: &mut dyn hardy_bpa::stream::Receiver<hardy_bpa::cla::Segment>,
    ) -> hardy_bpa::cla::Result<()> {
        unreachable!("forward tests never dispatch inbound bundles");
    }

    async fn add_peer(
        &self,
        cla_addr: ClaAddress,
        _node_ids: &[NodeId],
    ) -> hardy_bpa::cla::Result<bool> {
        self.0.lock().unwrap().push(cla_addr);
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

// Registered CLA with a single peer inbox, plus the ClaAddress the CLA
// announced for that inbox.
async fn registered_cla(inbox: &Path) -> (Cla, ClaAddress) {
    let peer: NodeId = "ipn:9.0".parse::<NodeId>().unwrap();
    let cla = Cla::new(&Config {
        outbox: None,
        peers: HashMap::from([(peer, inbox.to_path_buf())]),
    })
    .unwrap();

    let peers = Arc::new(Mutex::new(Vec::new()));
    cla.on_register(Box::new(StubSink(peers.clone())), &[])
        .await;

    let addr = peers
        .lock()
        .unwrap()
        .first()
        .cloned()
        .expect("no peer registered");
    (cla, addr)
}

fn dtn_bundle_id() -> hardy_bpv7::bundle::Id {
    hardy_bpv7::bundle::Id {
        source: "dtn://source-node/some/service".parse().unwrap(),
        ..Default::default()
    }
}

// forward() writes the bundle bytes into the peer inbox under a sanitized,
// single-component filename.
#[tokio::test]
async fn test_forward_writes_inbox() {
    let dir = tempfile::tempdir().unwrap();
    let (cla, addr) = registered_cla(dir.path()).await;

    let bundle = b"raw-bundle-bytes";
    let result = cla
        .forward(
            None,
            &addr,
            &dtn_bundle_id(),
            bundle.len() as u64,
            &mut hardy_bpa::Bytes::from_static(bundle),
        )
        .await
        .unwrap();
    assert!(matches!(result, ForwardBundleResult::Sent));

    let entries: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|e| e.unwrap())
        .collect();
    assert_eq!(entries.len(), 1, "exactly one file written to the inbox");
    assert!(entries[0].file_type().unwrap().is_file());

    let name = entries[0].file_name().into_string().unwrap();
    assert!(
        !name.contains(['\\', '/', ':', ' ']),
        "filename must be sanitized: {name:?}"
    );
    assert_eq!(std::fs::read(entries[0].path()).unwrap(), bundle);
}

// A fragment's filename carries the `_fragment_<offset>` suffix.
#[tokio::test]
async fn test_forward_fragment_suffix() {
    let dir = tempfile::tempdir().unwrap();
    let (cla, addr) = registered_cla(dir.path()).await;

    let bundle_id = hardy_bpv7::bundle::Id {
        fragment_info: Some(FragmentInfo {
            offset: 42,
            total_adu_length: 100,
        }),
        ..dtn_bundle_id()
    };

    let result = cla
        .forward(
            None,
            &addr,
            &bundle_id,
            4,
            &mut hardy_bpa::Bytes::from_static(b"frag"),
        )
        .await
        .unwrap();
    assert!(matches!(result, ForwardBundleResult::Sent));

    let entry = std::fs::read_dir(dir.path())
        .unwrap()
        .next()
        .unwrap()
        .unwrap();
    let name = entry.file_name().into_string().unwrap();
    assert!(
        name.ends_with("_fragment_42"),
        "fragment filename must carry the offset suffix: {name:?}"
    );
}

// An address that is not a configured inbox is NoNeighbour, and nothing is
// written.
#[tokio::test]
async fn test_forward_unknown_address_is_no_neighbour() {
    let dir = tempfile::tempdir().unwrap();
    let (cla, _addr) = registered_cla(dir.path()).await;

    let unknown = ClaAddress::Private(hardy_bpa::Bytes::from_static(
        b"/definitely/not/a/registered/inbox",
    ));
    let result = cla
        .forward(
            None,
            &unknown,
            &dtn_bundle_id(),
            4,
            &mut hardy_bpa::Bytes::from_static(b"data"),
        )
        .await
        .unwrap();
    assert!(matches!(result, ForwardBundleResult::NoNeighbour));
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
}

// A failed inbox write surfaces as Error::Internal.
#[tokio::test]
async fn test_forward_write_failure_is_internal_error() {
    let dir = tempfile::tempdir().unwrap();
    let inbox = dir.path().join("inbox");
    let (cla, addr) = registered_cla(&inbox).await;

    std::fs::remove_dir_all(&inbox).unwrap();

    let Err(err) = cla
        .forward(
            None,
            &addr,
            &dtn_bundle_id(),
            4,
            &mut hardy_bpa::Bytes::from_static(b"data"),
        )
        .await
    else {
        panic!("expected a write failure");
    };
    assert!(
        matches!(err, hardy_bpa::cla::Error::Internal(_)),
        "got {err:?}"
    );
}

// forward() before on_register is Disconnected.
#[tokio::test]
async fn test_forward_before_register_is_disconnected() {
    let dir = tempfile::tempdir().unwrap();
    let cla = Cla::new(&Config {
        outbox: None,
        peers: HashMap::from([("ipn:9.0".parse().unwrap(), dir.path().to_path_buf())]),
    })
    .unwrap();

    let addr = ClaAddress::Private(hardy_bpa::Bytes::from_static(b"unused"));
    let Err(err) = cla
        .forward(
            None,
            &addr,
            &dtn_bundle_id(),
            4,
            &mut hardy_bpa::Bytes::from_static(b"data"),
        )
        .await
    else {
        panic!("expected an error before registration");
    };
    assert!(
        matches!(err, hardy_bpa::cla::Error::Disconnected),
        "got {err:?}"
    );
}
