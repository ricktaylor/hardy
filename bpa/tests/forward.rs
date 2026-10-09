//! Integration tests for `Cla::forward` — the streamed egress door — and
//! `stream::buffer_stream`, the whole-buffer convenience used by CLAs that
//! need a contiguous bundle.

#[cfg(feature = "rfc9173")]
use core::num::NonZeroU8;
use core::num::NonZeroU64;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

#[cfg(feature = "rfc9173")]
use hardy_bpa::keys::KeyProvider;
use hardy_bpa::{
    Bytes, async_trait,
    bpa::{Bpa, BpaRegistration},
    cla::{self, Cla, ClaInit},
    services,
    stream::{Receiver, Segment},
};
use hardy_bpv7::{
    block,
    eid::{Eid, IpnNodeId, NodeId},
};
#[cfg(feature = "rfc9173")]
use hardy_bpv7::{
    bpsec::{
        DecryptingReader,
        encryptor::{self, Encryptor},
        key::{EncAlgorithm, Key, KeyAlgorithm, KeySet, KeySource, Operation, Type},
        rfc9173::ScopeFlags,
        signer::{self, Signer},
    },
    builder::Builder,
    bundle::Flags as BundleFlags,
    creation_timestamp::CreationTimestamp,
    hop_info::HopInfo,
    parse::Parsed,
    status_report::{AdministrativeRecord, ReasonCode},
};
#[cfg(feature = "rfc9173")]
use rand::{TryRng, rngs::SysRng};

// ---------------------------------------------------------------------------
// Events observed by the mock CLAs
// ---------------------------------------------------------------------------

enum Event {
    /// The buffering CLA assembled the whole bundle.
    Forward(Bytes),
    /// The streaming CLA pulled the stream to completion.
    Streamed {
        segments: Vec<Segment>,
        total_len: u64,
    },
    /// The forward attempt failed.
    Failed,
}

// ---------------------------------------------------------------------------
// Mock CLAs
// ---------------------------------------------------------------------------

/// Consumes the stream segment by segment, recording every segment it pulls.
struct StreamingCla {
    sink: hardy_async::sync::spin::Once<Box<dyn cla::Sink>>,
    events_tx: flume::Sender<Event>,
    /// When set, the first `forward` call fails without pulling.
    flaky: AtomicBool,
}

impl StreamingCla {
    fn new(flaky: bool) -> (Arc<Self>, flume::Receiver<Event>) {
        let (tx, rx) = flume::bounded(16);
        (
            Arc::new(Self {
                sink: hardy_async::sync::spin::Once::new(),
                events_tx: tx,
                flaky: AtomicBool::new(flaky),
            }),
            rx,
        )
    }
}

#[async_trait]
impl cla::Cla for StreamingCla {
    async fn on_register(
        &self,
        sink: Box<dyn cla::Sink>,
        _node_ids: &[NodeId],
        _max_bundle_size: Option<NonZeroU64>,
    ) {
        self.sink.call_once(|| sink);
    }

    async fn on_unregister(&self) {}

    async fn forward(
        &self,
        _lane: Option<u32>,
        _cla_addr: &cla::ClaAddress,
        _bundle_id: &hardy_bpv7::bundle::Id,
        total_len: u64,
        stream: &mut dyn Receiver<Segment>,
    ) -> cla::Result<cla::ForwardBundleResult> {
        if self.flaky.swap(false, Ordering::SeqCst) {
            let _ = self.events_tx.send(Event::Failed);
            // Any failure but a cancellation parks the bundle.
            return Err(cla::Error::Internal("the transfer failed".into()));
        }
        let mut segments = Vec::new();
        loop {
            match stream.recv().await {
                Ok(segment @ Segment::Next(_)) => segments.push(segment),
                Ok(segment @ Segment::Final(_)) => {
                    segments.push(segment);
                    break;
                }
                Err(_) => {
                    let _ = self.events_tx.send(Event::Failed);
                    return Err(cla::Error::StreamCancelled);
                }
            }
        }
        let _ = self.events_tx.send(Event::Streamed {
            segments,
            total_len,
        });
        Ok(cla::ForwardBundleResult::Sent)
    }
}

/// Buffers the stream into a contiguous bundle via `stream::buffer_stream`
/// — the shape every whole-buffer CLA takes on the streamed-only door.
struct BufferedCla {
    sink: hardy_async::sync::spin::Once<Box<dyn cla::Sink>>,
    events_tx: flume::Sender<Event>,
}

impl BufferedCla {
    fn new() -> (Arc<Self>, flume::Receiver<Event>) {
        let (tx, rx) = flume::bounded(16);
        (
            Arc::new(Self {
                sink: hardy_async::sync::spin::Once::new(),
                events_tx: tx,
            }),
            rx,
        )
    }
}

#[async_trait]
impl cla::Cla for BufferedCla {
    async fn on_register(
        &self,
        sink: Box<dyn cla::Sink>,
        _node_ids: &[NodeId],
        _max_bundle_size: Option<NonZeroU64>,
    ) {
        self.sink.call_once(|| sink);
    }

    async fn on_unregister(&self) {}

    async fn forward(
        &self,
        _lane: Option<u32>,
        _cla_addr: &cla::ClaAddress,
        _bundle_id: &hardy_bpv7::bundle::Id,
        total_len: u64,
        stream: &mut dyn Receiver<Segment>,
    ) -> cla::Result<cla::ForwardBundleResult> {
        let bundle = hardy_bpa::stream::buffer_stream(stream, total_len).await?;
        let _ = self.events_tx.send(Event::Forward(bundle));
        Ok(cla::ForwardBundleResult::Sent)
    }
}

// ---------------------------------------------------------------------------
// Minimal application to originate bundles
// ---------------------------------------------------------------------------

struct SendOnlyApp {
    sink: hardy_async::sync::spin::Once<Box<dyn services::ApplicationSink>>,
}

impl SendOnlyApp {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            sink: hardy_async::sync::spin::Once::new(),
        })
    }
}

#[async_trait]
impl services::Application for SendOnlyApp {
    async fn on_register(&self, _source: &Eid, sink: Box<dyn services::ApplicationSink>) {
        self.sink.call_once(|| sink);
    }

    async fn on_unregister(&self) {}

    async fn on_deliver(
        &self,
        _bundle_id: &hardy_bpv7::bundle::Id,
        _expiry: time::OffsetDateTime,
        _ack_requested: bool,
        _total_len: u64,
        _stream: &mut dyn Receiver<Segment>,
    ) -> services::Result<()> {
        Ok(())
    }

    async fn on_status_notify(
        &self,
        _bundle_id: &hardy_bpv7::bundle::Id,
        _from: &Eid,
        _kind: services::StatusNotify,
        _reason: hardy_bpv7::status_report::ReasonCode,
        _timestamp: Option<time::OffsetDateTime>,
    ) {
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn build_bundle(
    source: &Eid,
    destination: &Eid,
    payload: &[u8],
) -> (hardy_bpv7::bundle::Bundle, Bytes) {
    let (bundle, data) = hardy_bpv7::builder::Builder::new(source.clone(), destination.clone())
        .with_payload(std::borrow::Cow::Borrowed(payload))
        .build(hardy_bpv7::creation_timestamp::CreationTimestamp::now())
        .expect("Failed to build bundle");
    (bundle, Bytes::from(data))
}

/// A pre-filled segment stream. The sender is dropped on return, so a
/// sequence not ending in `Final` reads as a truncated stream.
async fn feed(segments: Vec<Segment>) -> hardy_async::channel::Receiver<Segment> {
    let (tx, rx) = hardy_async::channel::bounded(segments.len().max(1));
    for segment in segments {
        hardy_async::channel::Sender::send(&tx, segment)
            .await
            .unwrap();
    }
    rx
}

fn remote_node(node_number: u32) -> NodeId {
    NodeId::Ipn(IpnNodeId {
        allocator_id: 0,
        node_number,
    })
}

async fn recv_event(rx: &flume::Receiver<Event>, secs: u64) -> Event {
    tokio::time::timeout(tokio::time::Duration::from_secs(secs), rx.recv_async())
        .await
        .expect("Timed out waiting for CLA event")
        .expect("CLA event channel closed")
}

/// Sends `payload` to ipn:0.2.99 via `app`, returning the destination EID.
async fn originate(app: &SendOnlyApp, payload: &'static [u8]) -> Eid {
    let dest: Eid = "ipn:0.2.99".parse().unwrap();
    app.sink
        .get()
        .unwrap()
        .send(
            dest.clone(),
            Bytes::from_static(payload),
            core::time::Duration::from_secs(3600),
            None,
        )
        .await
        .unwrap();
    dest
}

// ---------------------------------------------------------------------------
// Full-path tests: dispatcher -> forward
// ---------------------------------------------------------------------------

/// A streaming CLA receives the rebuilt bundle as segments, one per chunk of
/// the egress rebuild, ending in a `Final`, with an exact `total_len`; the
/// bundle carries this node's Previous Node and the payload as stored.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn streaming_cla_receives_the_rebuild_as_segments() {
    let bpa = Bpa::builder().build().await.unwrap();
    bpa.start(false).await;

    let (cla, events_rx) = StreamingCla::new(false);
    bpa.register_cla("stream".to_string(), cla.clone(), None, ClaInit::default())
        .await
        .unwrap();
    cla.sink
        .get()
        .unwrap()
        .add_peer(
            cla::ClaAddress::Private("peer".as_bytes().into()),
            &[remote_node(2)],
        )
        .await
        .unwrap();

    let app = SendOnlyApp::new();
    let source_eid = bpa
        .register_application(hardy_bpv7::eid::Service::Ipn(42), app.clone())
        .await
        .unwrap();
    let dest = originate(&app, b"Hello remote").await;

    let Event::Streamed {
        segments,
        total_len,
    } = recv_event(&events_rx, 5).await
    else {
        panic!("Expected the streamed door, got another event");
    };
    let (last, init) = segments.split_last().expect("at least the Final");
    assert!(
        !init.is_empty() && init.iter().all(|s| matches!(s, Segment::Next(_))),
        "the rebuild travels as its chunks, not one flattened buffer"
    );
    assert!(matches!(last, Segment::Final(_)));
    let data: Vec<u8> = segments
        .iter()
        .flat_map(|(Segment::Next(data) | Segment::Final(data))| data.iter().copied())
        .collect();
    assert_eq!(total_len, data.len() as u64);

    let parsed = hardy_bpv7::parse::parse(data.into()).expect("Failed to parse forwarded bundle");
    assert_eq!(parsed.bundle.primary.id.source, source_eid);
    assert_eq!(parsed.bundle.primary.destination, dest);
    let Eid::Ipn { fqnn, .. } = source_eid else {
        panic!("an ipn application endpoint");
    };
    let previous = parsed
        .bundle
        .blocks
        .values()
        .find(|b| b.block_type == block::Type::PreviousNode)
        .expect("the forwarder writes a Previous Node")
        .extract::<Eid>(&parsed.data)
        .expect("the previous node decodes")
        .expect("the previous node is resident");
    assert_eq!(
        previous,
        Eid::Ipn {
            fqnn,
            service_number: 0
        }
    );
    assert_eq!(
        parsed.bundle.blocks[&1].payload(&parsed.data),
        Some(&b"Hello remote"[..])
    );

    assert!(events_rx.is_empty());
    bpa.shutdown().await;
}

/// A CLA that buffers via `stream::buffer_stream` still receives the whole
/// bundle.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn buffered_cla_receives_whole_bundle() {
    let bpa = Bpa::builder().build().await.unwrap();
    bpa.start(false).await;

    let (cla, events_rx) = BufferedCla::new();
    bpa.register_cla(
        "buffered".to_string(),
        cla.clone(),
        None,
        ClaInit::default(),
    )
    .await
    .unwrap();
    cla.sink
        .get()
        .unwrap()
        .add_peer(
            cla::ClaAddress::Private("peer".as_bytes().into()),
            &[remote_node(2)],
        )
        .await
        .unwrap();

    let app = SendOnlyApp::new();
    let source_eid = bpa
        .register_application(hardy_bpv7::eid::Service::Ipn(42), app.clone())
        .await
        .unwrap();
    let dest = originate(&app, b"Hello adapter").await;

    let Event::Forward(data) = recv_event(&events_rx, 5).await else {
        panic!("Expected the buffering CLA to assemble the bundle");
    };
    let parsed = hardy_bpv7::parse::parse(data.clone()).expect("Failed to parse forwarded bundle");
    assert_eq!(parsed.bundle.primary.id.source, source_eid);
    assert_eq!(parsed.bundle.primary.destination, dest);

    bpa.shutdown().await;
}

/// A failed streamed attempt takes the established requeue path: the bundle
/// returns to Waiting and a routing change re-dispatches it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_streamed_forward_is_requeued_and_retried() {
    let bpa = Bpa::builder().build().await.unwrap();
    bpa.start(false).await;

    let (cla, events_rx) = StreamingCla::new(true);
    bpa.register_cla("flaky".to_string(), cla.clone(), None, ClaInit::default())
        .await
        .unwrap();
    cla.sink
        .get()
        .unwrap()
        .add_peer(
            cla::ClaAddress::Private("peer-a".as_bytes().into()),
            &[remote_node(2)],
        )
        .await
        .unwrap();

    let app = SendOnlyApp::new();
    bpa.register_application(hardy_bpv7::eid::Service::Ipn(42), app.clone())
        .await
        .unwrap();
    originate(&app, b"Try again").await;

    assert!(matches!(recv_event(&events_rx, 5).await, Event::Failed));

    // Nudge the RIB so the Waiting bundle is re-polled. The failed attempt
    // returns the bundle to Waiting *after* the CLA reports the failure, so
    // a single nudge can race it; each fresh peer re-triggers the poll.
    let mut retry = None;
    for i in 0.. {
        cla.sink
            .get()
            .unwrap()
            .add_peer(
                cla::ClaAddress::Private(format!("peer-{i}").into_bytes().into()),
                &[remote_node(2)],
            )
            .await
            .unwrap();
        if let Ok(Ok(event)) = tokio::time::timeout(
            tokio::time::Duration::from_millis(500),
            events_rx.recv_async(),
        )
        .await
        {
            retry = Some(event);
            break;
        }
        assert!(i < 20, "Timed out waiting for the retry");
    }
    let Some(Event::Streamed { segments, .. }) = retry else {
        panic!("Expected a successful retry through the streamed door");
    };
    assert!(matches!(segments.last(), Some(Segment::Final(_))));

    bpa.shutdown().await;
}

// ---------------------------------------------------------------------------
// Direct-call tests: a buffering CLA over `stream::buffer_stream`
// ---------------------------------------------------------------------------

fn direct_call_fixture() -> (
    Arc<BufferedCla>,
    flume::Receiver<Event>,
    hardy_bpv7::bundle::Bundle,
    Bytes,
    cla::ClaAddress,
) {
    let (cla, events_rx) = BufferedCla::new();
    let (bundle, data) = build_bundle(
        &"ipn:0.1.1".parse().unwrap(),
        &"ipn:0.2.99".parse().unwrap(),
        b"payload",
    );
    let addr = cla::ClaAddress::Private("x".as_bytes().into());
    (cla, events_rx, bundle, data, addr)
}

/// The buffering path reassembles a multi-segment stream.
#[tokio::test]
async fn buffering_cla_concats_multi_segment_stream() {
    let (cla, events_rx, bundle, data, addr) = direct_call_fixture();
    let (head, tail) = (data.slice(..10), data.slice(10..));
    let mut rx = feed(vec![Segment::Next(head), Segment::Final(tail)]).await;

    let result = cla
        .forward(None, &addr, &bundle.primary.id, data.len() as u64, &mut rx)
        .await
        .unwrap();
    assert!(matches!(result, cla::ForwardBundleResult::Sent));

    let Ok(Event::Forward(received)) = events_rx.try_recv() else {
        panic!("Expected forward to receive the reassembled bundle");
    };
    assert_eq!(received, data);
}

/// A single-`Final` stream passes through the buffering path zero-copy.
#[tokio::test]
async fn buffering_cla_is_zero_copy_for_single_final() {
    let (cla, events_rx, bundle, data, addr) = direct_call_fixture();
    let mut rx = feed(vec![Segment::Final(data.clone())]).await;

    cla.forward(None, &addr, &bundle.primary.id, data.len() as u64, &mut rx)
        .await
        .unwrap();

    let Ok(Event::Forward(received)) = events_rx.try_recv() else {
        panic!("Expected forward to receive the bundle");
    };
    assert_eq!(received.as_ptr(), data.as_ptr());
}

/// A truncated stream is an error, and no partial bundle reaches the
/// transport.
#[tokio::test]
async fn buffering_cla_truncated_stream_is_cancelled() {
    let (cla, events_rx, bundle, data, addr) = direct_call_fixture();
    let mut rx = feed(vec![Segment::Next(data.slice(..4))]).await;

    let Err(err) = cla
        .forward(None, &addr, &bundle.primary.id, data.len() as u64, &mut rx)
        .await
    else {
        panic!("Expected a truncated stream to fail");
    };
    assert!(matches!(err, cla::Error::StreamCancelled));
    assert!(events_rx.is_empty());
}

/// A stream completing with fewer bytes than the declared `total_len` is
/// rejected — no short transfer reaches the transport.
#[tokio::test]
async fn buffering_cla_rejects_under_delivering_stream() {
    let (cla, events_rx, bundle, data, addr) = direct_call_fixture();
    let short = data.slice(..data.len() - 1);
    let mut rx = feed(vec![Segment::Final(short.clone())]).await;

    let Err(err) = cla
        .forward(None, &addr, &bundle.primary.id, data.len() as u64, &mut rx)
        .await
    else {
        panic!("Expected an under-delivering stream to fail");
    };
    assert!(matches!(
        err,
        cla::Error::PayloadUnderrun { size, expected }
            if size == short.len() as u64 && expected == data.len() as u64
    ));
    assert!(events_rx.is_empty());
}

/// A stream exceeding the declared `total_len` is rejected.
#[tokio::test]
async fn buffering_cla_rejects_stream_exceeding_total_len() {
    let (cla, events_rx, bundle, data, addr) = direct_call_fixture();
    let mut rx = feed(vec![Segment::Final(data.clone())]).await;

    let Err(err) = cla
        .forward(None, &addr, &bundle.primary.id, 4, &mut rx)
        .await
    else {
        panic!("Expected an oversize stream to fail");
    };
    assert!(matches!(
        err,
        cla::Error::PayloadTooLarge { size, max: 4 } if size > 4
    ));
    assert!(events_rx.is_empty());
}

// ---------------------------------------------------------------------------
// Legacy IPN re-encode built-in (the per-hop rewrite stage)
// ---------------------------------------------------------------------------

/// Builds a BPA on an allocator-1 node — its 3-element IPN EIDs change bytes
/// under the legacy re-encode — with the given legacy-peer patterns, wired
/// to a buffering CLA and a send-only application.
async fn legacy_fixture(patterns: &[&str]) -> (Bpa, Arc<SendOnlyApp>, flume::Receiver<Event>, Eid) {
    let node_ids = hardy_bpa::node_ids::NodeIds::try_from(
        [NodeId::Ipn(IpnNodeId {
            allocator_id: 1,
            node_number: 1,
        })]
        .as_slice(),
    )
    .unwrap();
    let bpa = Bpa::builder()
        .node_ids(node_ids)
        .ipn_legacy_peers(patterns.iter().map(|p| p.parse().unwrap()).collect())
        .build()
        .await
        .unwrap();
    bpa.start(false).await;

    let (cla, events_rx) = BufferedCla::new();
    bpa.register_cla(
        "buffer".to_string(),
        cla.clone(),
        None,
        cla::ClaInit::default(),
    )
    .await
    .unwrap();
    cla.sink
        .get()
        .unwrap()
        .add_peer(
            cla::ClaAddress::Private("peer".as_bytes().into()),
            &[NodeId::Ipn(IpnNodeId {
                allocator_id: 1,
                node_number: 2,
            })],
        )
        .await
        .unwrap();

    let app = SendOnlyApp::new();
    let source_eid = bpa
        .register_application(hardy_bpv7::eid::Service::Ipn(42), app.clone())
        .await
        .unwrap();
    (bpa, app, events_rx, source_eid)
}

/// A next hop matching a configured legacy-peer pattern receives the bundle
/// with `Ipn` source and destination re-encoded as `LegacyIpn` on the wire.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn legacy_peer_receives_two_element_encoding() {
    let (bpa, app, events_rx, _) = legacy_fixture(&["ipn:1.2.*"]).await;

    let dest: Eid = "ipn:1.2.99".parse().unwrap();
    app.sink
        .get()
        .unwrap()
        .send(
            dest,
            Bytes::from_static(b"legacy"),
            core::time::Duration::from_secs(3600),
            None,
        )
        .await
        .unwrap();

    let Event::Forward(data) = recv_event(&events_rx, 5).await else {
        panic!("Expected a forwarded bundle");
    };
    let parsed = hardy_bpv7::parse::parse(data).expect("Failed to parse forwarded bundle");
    assert!(
        matches!(parsed.bundle.primary.id.source, Eid::LegacyIpn { .. }),
        "source must be re-encoded 2-element, got {:?}",
        parsed.bundle.primary.id.source
    );
    assert!(
        matches!(parsed.bundle.primary.destination, Eid::LegacyIpn { .. }),
        "destination must be re-encoded 2-element, got {:?}",
        parsed.bundle.primary.destination
    );

    assert!(events_rx.is_empty());
    bpa.shutdown().await;
}

/// A next hop matching no configured pattern receives the canonical
/// 3-element encoding untouched.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn non_legacy_peer_keeps_canonical_encoding() {
    let (bpa, app, events_rx, source_eid) = legacy_fixture(&["ipn:9.9.*"]).await;

    let dest: Eid = "ipn:1.2.99".parse().unwrap();
    app.sink
        .get()
        .unwrap()
        .send(
            dest.clone(),
            Bytes::from_static(b"canonical"),
            core::time::Duration::from_secs(3600),
            None,
        )
        .await
        .unwrap();

    let Event::Forward(data) = recv_event(&events_rx, 5).await else {
        panic!("Expected a forwarded bundle");
    };
    let parsed = hardy_bpv7::parse::parse(data).expect("Failed to parse forwarded bundle");
    assert_eq!(parsed.bundle.primary.id.source, source_eid);
    assert_eq!(parsed.bundle.primary.destination, dest);

    assert!(events_rx.is_empty());
    bpa.shutdown().await;
}

// A relay on allocator 1 whose one peer, ipn:1.2, needs legacy IPN
// encoding, with status reports on so that a drop's reason reaches the peer.
#[cfg(feature = "rfc9173")]
async fn legacy_relay() -> (Bpa, Arc<BufferedCla>, flume::Receiver<Event>) {
    let bpa = Bpa::builder()
        .node_ids(
            hardy_bpa::node_ids::NodeIds::try_from([NodeId::Ipn(relay_node(1))].as_slice())
                .unwrap(),
        )
        .ipn_legacy_peers(vec!["ipn:1.2.*".parse().unwrap()])
        .status_reports(true)
        .build()
        .await
        .unwrap();
    bpa.start(false).await;
    let (cla, events_rx) = BufferedCla::new();
    bpa.register_cla("cla".to_string(), cla.clone(), None, ClaInit::default())
        .await
        .unwrap();
    cla.sink
        .get()
        .unwrap()
        .add_peer(
            cla::ClaAddress::Private("peer".as_bytes().into()),
            &[NodeId::Ipn(relay_node(2))],
        )
        .await
        .unwrap();
    (bpa, cla, events_rx)
}

// A bundle from ipn:1.3.1 to an endpoint behind the legacy peer, asking for
// deletion reports there: (the parsed bundle, its Hop Count block number).
#[cfg(feature = "rfc9173")]
fn legacy_bound_bundle() -> (Parsed, u64) {
    let (_, data) = Builder::new("ipn:1.3.1".parse().unwrap(), "ipn:1.2.99".parse().unwrap())
        .with_flags(BundleFlags {
            delete_report_requested: true,
            ..Default::default()
        })
        .with_report_to("ipn:1.2.9".parse().unwrap())
        .with_hop_count(&HopInfo {
            limit: NonZeroU8::new(64).unwrap(),
            count: 0,
        })
        .with_payload(b"legacy bound".as_slice().into())
        .build(CreationTimestamp::now())
        .expect("build the bundle");
    let built = hardy_bpv7::parse::parse(Bytes::from(data)).unwrap();
    let hop_count = *built
        .bundle
        .blocks
        .iter()
        .find(|(_, b)| b.block_type == block::Type::HopCount)
        .expect("the bundle carries a Hop Count block")
        .0;
    (built, hop_count)
}

// Hand `received` to the relay through the legacy peer's CLA.
#[cfg(feature = "rfc9173")]
async fn receive(cla: &BufferedCla, received: &Parsed) {
    assert_eq!(
        cla.sink
            .get()
            .unwrap()
            .dispatch(None, None, &mut received.data.clone())
            .await
            .unwrap(),
        cla::Acceptance::Accepted
    );
}

// The bundle never leaves the node: the first thing the peer sees is the
// deletion report, giving `UnexpectedSecurityOperation`.
#[cfg(feature = "rfc9173")]
async fn assert_dropped_as_unexpected(
    bpa: Bpa,
    events_rx: flume::Receiver<Event>,
    received: &Parsed,
) {
    // The timeout only bounds a regression.
    let Event::Forward(report) = recv_event(&events_rx, 5).await else {
        panic!("Expected the deletion report");
    };
    let report = hardy_bpv7::parse::parse(report).expect("the report parses");
    assert!(
        report.bundle.primary.flags.is_admin_record,
        "only a status report may reach the legacy peer"
    );
    let AdministrativeRecord::BundleStatusReport(report) = report.bundle.blocks[&1]
        .extract::<AdministrativeRecord>(&report.data)
        .expect("the payload is an administrative record")
        .expect("the report payload is resident");
    assert_eq!(report.bundle_id, received.bundle.primary.id);
    assert!(report.deleted.is_some());
    assert_eq!(report.reason, ReasonCode::UnexpectedSecurityOperation);

    // shutdown() joins the pools: the bundle itself never left the node.
    bpa.shutdown().await;
    assert!(events_rx.is_empty());
}

// The bundle itself, not a deletion report, reaches the peer re-encoded for
// it, both EIDs in the two-element legacy form.
#[cfg(feature = "rfc9173")]
async fn assert_forwarded_legacy(
    bpa: Bpa,
    events_rx: flume::Receiver<Event>,
    received: &Parsed,
) -> Parsed {
    // The timeout only bounds a regression.
    let Event::Forward(data) = recv_event(&events_rx, 5).await else {
        panic!("Expected the forwarded bundle");
    };
    let forwarded = hardy_bpv7::parse::parse(data).expect("the forwarded bundle parses");
    assert!(
        !forwarded.bundle.primary.flags.is_admin_record,
        "the bundle itself, not a status report, reaches the peer"
    );
    assert_eq!(
        forwarded.bundle.primary.id.timestamp, received.bundle.primary.id.timestamp,
        "the forwarded bundle is the received one"
    );
    // The re-encode keeps the node and service numbers.
    let legacy = |eid: &Eid| match eid {
        Eid::Ipn {
            fqnn,
            service_number,
        } => Eid::LegacyIpn {
            fqnn: *fqnn,
            service_number: *service_number,
        },
        other => panic!("the received bundle carries ipn EIDs, not {other}"),
    };
    assert_eq!(
        forwarded.bundle.primary.id.source,
        legacy(&received.bundle.primary.id.source)
    );
    assert_eq!(
        forwarded.bundle.primary.destination,
        legacy(&received.bundle.primary.destination)
    );
    // shutdown() joins the pools. The bundle leaves once; the deletion
    // report its completed forward sends (it requests one) follows it
    // unless shutdown() unregisters the CLA first.
    bpa.shutdown().await;
    for event in events_rx.drain() {
        let Event::Forward(data) = event else {
            panic!("only the bundle and its reports leave the node");
        };
        let report = hardy_bpv7::parse::parse(data).expect("the report parses");
        assert!(
            report.bundle.primary.flags.is_admin_record,
            "the bundle leaves once"
        );
    }
    forwarded
}

// Sign `targets` of `bundle` under `scope` from its source: one BIB where
// the scope lets the targets share it, one each otherwise.
#[cfg(feature = "rfc9173")]
fn signed(bundle: &Parsed, targets: &[u64], scope: ScopeFlags, key: &Key) -> Parsed {
    let mut signing = Signer::new(&bundle.bundle, &bundle.data);
    for &target in targets {
        signing = signing
            .sign_block(
                target,
                signer::Context::HMAC_SHA2(scope.clone()),
                bundle.bundle.primary.id.source.clone(),
                key,
            )
            .map_err(|(_, e)| e)
            .expect("sign the target");
    }
    hardy_bpv7::parse::parse(Bytes::from(
        signing.rebuild().expect("rebuild the signed bundle"),
    ))
    .unwrap()
}

/// A legacy next hop needs a re-encoded primary block, and a BIB signs the
/// primary, so the signature could not survive: the bundle is dropped with
/// `UnexpectedSecurityOperation`. The one BIB also covers the Hop Count,
/// which the per-hop rewrite updates and strips from that BIB: the primary
/// stays signed regardless. The scope leaves the security header out, so
/// the two targets share the BIB.
#[cfg(feature = "rfc9173")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn legacy_peer_drops_a_bundle_with_a_signed_primary() {
    let (bpa, cla, events_rx) = legacy_relay().await;
    let (built, hop_count) = legacy_bound_bundle();
    let received = signed(
        &built,
        &[0, hop_count],
        ScopeFlags {
            include_security_header: false,
            ..ScopeFlags::default()
        },
        &sign_key(),
    );
    let block::BibCoverage::Some(bib) = received.bundle.blocks[&0].bib else {
        panic!("the primary is signed");
    };
    assert!(
        matches!(received.bundle.blocks[&hop_count].bib, block::BibCoverage::Some(n) if n == bib),
        "one BIB covers the primary and the Hop Count"
    );

    receive(&cla, &received).await;
    assert_dropped_as_unexpected(bpa, events_rx, &received).await;
}

/// No BIB targets the primary, but the payload's BIB has the primary in its
/// scope (RFC 9173's default), so the re-encode would break it: dropped.
#[cfg(feature = "rfc9173")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn legacy_peer_drops_a_bundle_whose_bib_scopes_the_primary() {
    let (bpa, cla, events_rx) = legacy_relay().await;
    let (built, _) = legacy_bound_bundle();
    let received = signed(&built, &[1], ScopeFlags::default(), &sign_key());

    receive(&cla, &received).await;
    assert_dropped_as_unexpected(bpa, events_rx, &received).await;
}

/// A payload BIB whose scope leaves the primary out survives the re-encode:
/// the bundle goes to the legacy peer, and the signature still verifies.
#[cfg(feature = "rfc9173")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn legacy_peer_takes_a_bundle_whose_bib_leaves_the_primary_out() {
    let (bpa, cla, events_rx) = legacy_relay().await;
    let (built, _) = legacy_bound_bundle();
    let key = sign_key();
    let received = signed(
        &built,
        &[1],
        ScopeFlags {
            include_primary_block: false,
            ..ScopeFlags::default()
        },
        &key,
    );

    receive(&cla, &received).await;
    let forwarded = assert_forwarded_legacy(bpa, events_rx, &received).await;
    assert_eq!(forwarded.bibs.len(), 1, "the payload's BIB travels");
    let deferred = hardy_bpv7::checks::verify_all_bibs(
        &forwarded.data,
        &KeySet::new(vec![key]),
        &forwarded.bundle.blocks,
        &forwarded.bibs,
        &Default::default(),
        &Default::default(),
    )
    .expect("the payload's signature verifies after the re-encode");
    assert!(deferred.is_empty(), "every target is resident");
}

/// A BCB whose AAD scope includes the primary (RFC 9173's default) counts
/// the same way: dropped.
#[cfg(feature = "rfc9173")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn legacy_peer_drops_a_bundle_whose_bcb_scopes_the_primary() {
    let (bpa, cla, events_rx) = legacy_relay().await;
    let (built, _) = legacy_bound_bundle();
    let key = enc_key();
    let received = hardy_bpv7::parse::parse(Bytes::from(
        Encryptor::new(&built.bundle, &built.data)
            .encrypt_block(
                1,
                encryptor::Context::AES_GCM(ScopeFlags::default()),
                built.bundle.primary.id.source.clone(),
                &key,
            )
            .map_err(|(_, e)| e)
            .expect("encrypt the payload")
            .rebuild()
            .expect("rebuild the encrypted bundle"),
    ))
    .unwrap();

    receive(&cla, &received).await;
    assert_dropped_as_unexpected(bpa, events_rx, &received).await;
}

/// A BCB whose AAD scope leaves the primary out survives the re-encode: the
/// bundle goes to the legacy peer, and its payload still decrypts.
#[cfg(feature = "rfc9173")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn legacy_peer_takes_a_bundle_whose_bcb_leaves_the_primary_out() {
    let (bpa, cla, events_rx) = legacy_relay().await;
    let (built, _) = legacy_bound_bundle();
    let key = enc_key();
    let received = hardy_bpv7::parse::parse(Bytes::from(
        Encryptor::new(&built.bundle, &built.data)
            .encrypt_block(
                1,
                encryptor::Context::AES_GCM(ScopeFlags {
                    include_primary_block: false,
                    ..ScopeFlags::default()
                }),
                built.bundle.primary.id.source.clone(),
                &key,
            )
            .map_err(|(_, e)| e)
            .expect("encrypt the payload")
            .rebuild()
            .expect("rebuild the encrypted bundle"),
    ))
    .unwrap();

    receive(&cla, &received).await;
    let forwarded = assert_forwarded_legacy(bpa, events_rx, &received).await;
    let keys = KeySet::new(vec![key]);
    let payload = DecryptingReader::new(
        &forwarded.bundle.blocks,
        &forwarded.data,
        &forwarded.bcbs,
        &keys,
    )
    .block_data(1)
    .expect("the payload decrypts after the re-encode")
    .expect("the payload is resident");
    assert_eq!(payload.as_ref(), b"legacy bound");
}

/// A default-scope BIB over the Hop Count alone goes with the per-hop
/// rewrite, which replaces that block and strips the emptied BIB before the
/// re-encode: nothing is left to break, so the bundle goes to the peer.
#[cfg(feature = "rfc9173")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn legacy_peer_takes_a_bundle_whose_only_bib_the_rewrite_removes() {
    let (bpa, cla, events_rx) = legacy_relay().await;
    let (built, hop_count) = legacy_bound_bundle();
    let received = signed(&built, &[hop_count], ScopeFlags::default(), &sign_key());

    receive(&cla, &received).await;
    let forwarded = assert_forwarded_legacy(bpa, events_rx, &received).await;
    assert!(
        !forwarded
            .bundle
            .blocks
            .values()
            .any(|b| b.block_type == block::Type::BlockIntegrity),
        "the emptied BIB went with the rewrite"
    );
}

// ---------------------------------------------------------------------------
// Relaying BPSec-protected bundles (the per-hop rewrite stage)
// ---------------------------------------------------------------------------

// Immaterial key values: generated per the no-literal-keys rule.
#[cfg(feature = "rfc9173")]
fn random_key(len: usize) -> Vec<u8> {
    let mut k = vec![0u8; len];
    SysRng.try_fill_bytes(&mut k).unwrap();
    k
}

#[cfg(feature = "rfc9173")]
fn sign_key() -> Key {
    Key {
        key_type: Type::octet_sequence(random_key(32)),
        key_algorithm: Some(KeyAlgorithm::HS256),
        enc_algorithm: None,
        operations: Some([Operation::Sign, Operation::Verify].into_iter().collect()),
        id: None,
        key_use: None,
    }
}

#[cfg(feature = "rfc9173")]
fn enc_key() -> Key {
    Key {
        key_type: Type::octet_sequence(random_key(32)),
        key_algorithm: None,
        enc_algorithm: Some(EncAlgorithm::A256GCM),
        operations: Some(
            [Operation::Encrypt, Operation::Decrypt]
                .into_iter()
                .collect(),
        ),
        id: None,
        key_use: None,
    }
}

#[cfg(feature = "rfc9173")]
fn relay_node(node_number: u32) -> IpnNodeId {
    IpnNodeId {
        allocator_id: 1,
        node_number,
    }
}

// A bundle as a relay receives it: the source signed then encrypted the
// payload (so its BIB is encrypted too), and an earlier relay has stamped
// PreviousNode and bumped the hop count. With `encrypt_hop_count`, the Hop
// Count block is BCB-encrypted as well, under `enc_key`.
#[cfg(feature = "rfc9173")]
fn protected_bundle(sign_key: &Key, enc_key: &Key, encrypt_hop_count: bool) -> Parsed {
    let source: Eid = "ipn:1.3.1".parse().unwrap();
    let (_, data) = Builder::new(source.clone(), "ipn:1.2.99".parse().unwrap())
        .with_hop_count(&HopInfo {
            limit: NonZeroU8::new(64).unwrap(),
            count: 1,
        })
        .add_extension_block(block::Type::PreviousNode)
        .expect("add the PreviousNode block")
        .build(hardy_cbor::encode::emit(&Eid::from(relay_node(4))).0.into())
        .with_payload(b"relay me".as_slice().into())
        .build(CreationTimestamp::now())
        .expect("build the bundle");
    let built = hardy_bpv7::parse::parse(Bytes::from(data)).unwrap();
    let signed = hardy_bpv7::parse::parse(Bytes::from(
        Signer::new(&built.bundle, &built.data)
            .sign_block(
                1,
                signer::Context::HMAC_SHA2(ScopeFlags::default()),
                source.clone(),
                sign_key,
            )
            .map_err(|(_, e)| e)
            .expect("sign the payload")
            .rebuild()
            .expect("rebuild the signed bundle"),
    ))
    .unwrap();
    let flags = ScopeFlags {
        include_security_header: false,
        ..ScopeFlags::default()
    };
    let mut encryptor = Encryptor::new(&signed.bundle, &signed.data)
        .encrypt_block(
            1,
            encryptor::Context::AES_GCM(flags.clone()),
            source.clone(),
            enc_key,
        )
        .map_err(|(_, e)| e)
        .expect("encrypt the payload (and so its covering BIB)");
    if encrypt_hop_count {
        let hop_count = *signed
            .bundle
            .blocks
            .iter()
            .find(|(_, b)| b.block_type == block::Type::HopCount)
            .expect("the bundle carries a Hop Count block")
            .0;
        encryptor = encryptor
            .encrypt_block(
                hop_count,
                encryptor::Context::AES_GCM(flags),
                source,
                enc_key,
            )
            .map_err(|(_, e)| e)
            .expect("encrypt the Hop Count block");
    }
    hardy_bpv7::parse::parse(Bytes::from(
        encryptor.rebuild().expect("rebuild the encrypted bundle"),
    ))
    .unwrap()
}

// A key provider lending the same keys for every bundle.
#[cfg(feature = "rfc9173")]
struct FixedKeys(Vec<Key>);

#[cfg(feature = "rfc9173")]
impl KeyProvider for FixedKeys {
    fn key_source(&self, _bundle: &hardy_bpv7::Bundle, _data: &[u8]) -> Box<dyn KeySource> {
        Box::new(KeySet::new(self.0.clone()))
    }
}

// Relays `received` through node 1.1 holding `keys` (none for a keyless
// relay) toward its peer node 1.2, returning the forwarded bundle.
#[cfg(feature = "rfc9173")]
async fn relay(received: &Bytes, keys: Vec<Key>) -> Parsed {
    let bpa = Bpa::builder()
        .node_ids(
            hardy_bpa::node_ids::NodeIds::try_from([NodeId::Ipn(relay_node(1))].as_slice())
                .unwrap(),
        )
        .key_provider(Arc::new(FixedKeys(keys)))
        .build()
        .await
        .unwrap();
    bpa.start(false).await;
    let (ingress, _) = BufferedCla::new();
    bpa.register_cla(
        "ingress".to_string(),
        ingress.clone(),
        None,
        ClaInit::default(),
    )
    .await
    .unwrap();
    let (egress, events_rx) = BufferedCla::new();
    bpa.register_cla(
        "egress".to_string(),
        egress.clone(),
        None,
        ClaInit::default(),
    )
    .await
    .unwrap();
    egress
        .sink
        .get()
        .unwrap()
        .add_peer(
            cla::ClaAddress::Private("peer".as_bytes().into()),
            &[NodeId::Ipn(relay_node(2))],
        )
        .await
        .unwrap();

    assert_eq!(
        ingress
            .sink
            .get()
            .unwrap()
            .dispatch(None, None, &mut received.clone())
            .await
            .unwrap(),
        cla::Acceptance::Accepted
    );

    // The timeout only bounds a regression: a relay that refuses the
    // bundle never offers it to the peer.
    let Event::Forward(forwarded) = recv_event(&events_rx, 5).await else {
        panic!("Expected the relay to forward the bundle");
    };

    // shutdown() joins the pools, so anything the relay was going to offer
    // has reached the CLA by the time it returns: nothing further arrived.
    bpa.shutdown().await;
    assert!(events_rx.is_empty());
    hardy_bpv7::parse::parse(forwarded).expect("the forwarded bundle parses")
}

// The block of `block_type` in `parsed`.
#[cfg(feature = "rfc9173")]
fn extension(parsed: &Parsed, block_type: block::Type) -> &block::Block {
    parsed
        .bundle
        .blocks
        .values()
        .find(|b| b.block_type == block_type)
        .expect("the block is present")
}

// Every BCB-covered or security block of `received` leaves the relay
// byte-identical, under the same block number.
#[cfg(feature = "rfc9173")]
fn assert_protected_blocks_untouched(received: &Parsed, out: &Parsed) {
    for (n, before) in received.bundle.blocks.iter().filter(|(_, b)| {
        b.bcb.is_some()
            || matches!(
                b.block_type,
                block::Type::BlockIntegrity | block::Type::BlockSecurity
            )
    }) {
        let after = out.bundle.blocks.get(n).expect("the block survives");
        assert_eq!(after.block_type, before.block_type);
        assert_eq!(
            after.payload(&out.data),
            before.payload(&received.data),
            "block {n} must leave the relay untouched"
        );
    }
}

// A keyless relay forwards a bundle whose payload was signed then
// encrypted (so its BIB is encrypted too) and which already carries a
// PreviousNode and a HopCount from earlier hops: both per-hop rewrites go
// ahead, and the payload, BIB and BCBs leave byte-identical, so the
// payload still decrypts under the original key downstream.
#[cfg(feature = "rfc9173")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn keyless_relay_forwards_a_signed_then_encrypted_bundle() {
    let enc_key = enc_key();
    let received = protected_bundle(&sign_key(), &enc_key, false);

    let out = relay(&received.data, Vec::new()).await;

    let hop_info = extension(&out, block::Type::HopCount)
        .extract::<HopInfo>(&out.data)
        .expect("the hop count decodes")
        .expect("the hop count is resident");
    assert_eq!(hop_info.count, 2, "the relay bumps the hop count");
    let previous = extension(&out, block::Type::PreviousNode)
        .extract::<Eid>(&out.data)
        .expect("the previous node decodes")
        .expect("the previous node is resident");
    assert_eq!(previous, Eid::from(relay_node(1)), "the relay names itself");

    assert_protected_blocks_untouched(&received, &out);

    let keys = KeySet::new(vec![enc_key]);
    let payload = DecryptingReader::new(&out.bundle.blocks, &out.data, &out.bcbs, &keys)
        .block_data(1)
        .expect("the payload decrypts downstream")
        .expect("the payload is resident");
    assert_eq!(payload.as_ref(), b"relay me");
}

// A keyless relay can't read an encrypted Hop Count. The increment is a
// SHOULD (RFC 9171 §4.4.3), so the bundle passes ingress and the block
// travels unchanged with its BCB; the PreviousNode is still replaced.
#[cfg(feature = "rfc9173")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn keyless_relay_forwards_an_encrypted_hop_count_unchanged() {
    let enc_key = enc_key();
    let received = protected_bundle(&sign_key(), &enc_key, true);
    assert!(
        extension(&received, block::Type::HopCount).bcb.is_some(),
        "the Hop Count arrives encrypted"
    );

    let out = relay(&received.data, Vec::new()).await;

    assert_protected_blocks_untouched(&received, &out);
    let previous = extension(&out, block::Type::PreviousNode)
        .extract::<Eid>(&out.data)
        .expect("the previous node decodes")
        .expect("the previous node is resident");
    assert_eq!(previous, Eid::from(relay_node(1)), "the relay names itself");
}

// A relay holding the decryption key decrypts the encrypted BIB at ingress
// and learns it covers only the payload, so the encrypted Hop Count is
// provably outside it. Required to update the block, the relay accepts its
// confidentiality operation and writes the increment in plaintext
// (forwarder policy); the payload's protection leaves untouched, and the
// payload still decrypts downstream.
#[cfg(feature = "rfc9173")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn key_holding_relay_updates_an_encrypted_hop_count_outside_the_bib() {
    let enc_key = enc_key();
    let received = protected_bundle(&sign_key(), &enc_key, true);
    let hop = *received
        .bundle
        .blocks
        .iter()
        .find(|(_, b)| b.block_type == block::Type::HopCount)
        .expect("the Hop Count block is present")
        .0;
    assert!(
        received.bundle.blocks[&hop].bcb.is_some(),
        "the Hop Count arrives encrypted"
    );

    let out = relay(&received.data, vec![enc_key.clone()]).await;

    let hop_count = extension(&out, block::Type::HopCount);
    assert!(
        hop_count.bcb.is_none(),
        "the relay writes the Hop Count in plaintext"
    );
    let hop_info = hop_count
        .extract::<HopInfo>(&out.data)
        .expect("the hop count decodes")
        .expect("the hop count is resident");
    assert_eq!(hop_info.count, 2, "the relay increments the count");
    assert!(
        out.bcbs
            .values()
            .all(|opset| !opset.operations().contains_key(&hop)),
        "no BCB names the Hop Count any more"
    );
    let previous = extension(&out, block::Type::PreviousNode)
        .extract::<Eid>(&out.data)
        .expect("the previous node decodes")
        .expect("the previous node is resident");
    assert_eq!(previous, Eid::from(relay_node(1)), "the relay names itself");

    // The payload's protection is untouched: every security block that
    // does not name the Hop Count leaves byte-identical.
    for (n, before) in received.bundle.blocks.iter().filter(|(n, b)| {
        b.block_type == block::Type::BlockIntegrity
            || (b.block_type == block::Type::BlockSecurity
                && !received.bcbs[n].operations().contains_key(&hop))
    }) {
        let after = out.bundle.blocks.get(n).expect("the block survives");
        assert_eq!(after.block_type, before.block_type);
        assert_eq!(
            after.payload(&out.data),
            before.payload(&received.data),
            "block {n} must leave the relay untouched"
        );
    }
    let keys = KeySet::new(vec![enc_key]);
    let payload = DecryptingReader::new(&out.bundle.blocks, &out.data, &out.bcbs, &keys)
        .block_data(1)
        .expect("the payload decrypts downstream")
        .expect("the payload is resident");
    assert_eq!(payload.as_ref(), b"relay me");
}
