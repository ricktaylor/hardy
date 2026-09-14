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

use hardy_bpa::{
    Bytes, async_trait,
    bpa::{Bpa, BpaRegistration},
    cla::{self, Cla, ClaInit},
    services,
    stream::{Receiver, Segment},
};
use hardy_bpv7::eid::{Eid, IpnNodeId, NodeId};
#[cfg(feature = "rfc9173")]
use hardy_bpv7::{
    block,
    bpsec::{
        DecryptingReader,
        encryptor::{self, Encryptor},
        key::{EncAlgorithm, Key, KeyAlgorithm, KeySet, Operation, Type},
        rfc9173::ScopeFlags,
        signer::{self, Signer},
    },
    builder::Builder,
    creation_timestamp::CreationTimestamp,
    hop_info::HopInfo,
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
            return Err(cla::Error::StreamCancelled);
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

/// Fails every `forward` call synchronously, without pulling.
struct FailingCla {
    sink: hardy_async::sync::spin::Once<Box<dyn cla::Sink>>,
    events_tx: flume::Sender<Event>,
}

impl FailingCla {
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
impl cla::Cla for FailingCla {
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
        _total_len: u64,
        _stream: &mut dyn Receiver<Segment>,
    ) -> cla::Result<cla::ForwardBundleResult> {
        let _ = self.events_tx.send(Event::Failed);
        Err(cla::Error::StreamCancelled)
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
            core::time::Duration::from_secs(3600),
            None,
            None,
            &mut Bytes::from_static(payload),
        )
        .await
        .unwrap();
    dest
}

// ---------------------------------------------------------------------------
// Full-path tests: dispatcher -> forward
// ---------------------------------------------------------------------------

/// A streaming CLA receives the whole bundle as a single `Final` segment
/// with an exact `total_len`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn streaming_cla_receives_single_final_segment() {
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
    assert_eq!(segments.len(), 1);
    let Segment::Final(data) = &segments[0] else {
        panic!("Expected a single Final segment");
    };
    assert_eq!(total_len, data.len() as u64);

    let parsed = hardy_bpv7::parse::parse(data.clone()).expect("Failed to parse forwarded bundle");
    assert_eq!(parsed.bundle.primary.id.source, source_eid);
    assert_eq!(parsed.bundle.primary.destination, dest);

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

/// A synchronous per-transfer failure parks only that bundle, with no inline
/// retry: a deterministic failure must not spin dispatch → forward → fail,
/// so exactly one attempt occurs until the next routing or link event.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_streamed_forward_does_not_retry_inline() {
    let bpa = Bpa::builder().build().await.unwrap();
    bpa.start(false).await;

    let (cla, events_rx) = FailingCla::new();
    bpa.register_cla("failing".to_string(), cla.clone(), None, ClaInit::default())
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
    originate(&app, b"One shot").await;

    assert!(matches!(recv_event(&events_rx, 5).await, Event::Failed));

    // The bundle is back in Waiting; with no routing or link event, no
    // further attempt may occur. shutdown() is the barrier: it joins the
    // pools, and the CLA mock records every attempt synchronously inside
    // forward(), so any wrong re-attempt is in events_rx by the time it
    // returns. No quiet window is involved.
    bpa.shutdown().await;
    assert!(
        events_rx.is_empty(),
        "A synchronous failure must not re-attempt without a routing event"
    );
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
            core::time::Duration::from_secs(3600),
            None,
            None,
            &mut Bytes::from_static(b"legacy"),
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
            core::time::Duration::from_secs(3600),
            None,
            None,
            &mut Bytes::from_static(b"canonical"),
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

// ---------------------------------------------------------------------------
// Relaying BPSec-protected bundles (the per-hop rewrite stage)
// ---------------------------------------------------------------------------

// A keyless relay forwards a bundle whose payload was signed then
// encrypted (so its BIB is encrypted too) and which already carries a
// PreviousNode and a HopCount from earlier hops: both per-hop rewrites go
// ahead, and the payload, BIB and BCBs leave byte-identical, so the
// payload still decrypts under the original key downstream.
#[cfg(feature = "rfc9173")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn keyless_relay_forwards_a_signed_then_encrypted_bundle() {
    let node = |node_number| IpnNodeId {
        allocator_id: 1,
        node_number,
    };
    // Immaterial key values: generated per the no-literal-keys rule.
    let random_key = |len| {
        let mut k = vec![0u8; len];
        SysRng.try_fill_bytes(&mut k).unwrap();
        k
    };
    let sign_key = Key {
        key_type: Type::octet_sequence(random_key(32)),
        key_algorithm: Some(KeyAlgorithm::HS256),
        enc_algorithm: None,
        operations: Some([Operation::Sign, Operation::Verify].into_iter().collect()),
        id: None,
        key_use: None,
    };
    let enc_key = Key {
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
    };

    // The source signs then encrypts the payload; an earlier relay has
    // already stamped PreviousNode and bumped the hop count.
    let source: Eid = "ipn:1.3.1".parse().unwrap();
    let (_, data) = Builder::new(source.clone(), "ipn:1.2.99".parse().unwrap())
        .with_hop_count(&HopInfo {
            limit: NonZeroU8::new(64).unwrap(),
            count: 1,
        })
        .add_extension_block(block::Type::PreviousNode)
        .expect("add the PreviousNode block")
        .build(hardy_cbor::encode::emit(&Eid::from(node(4))).0.into())
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
                &sign_key,
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
    let received = hardy_bpv7::parse::parse(Bytes::from(
        Encryptor::new(&signed.bundle, &signed.data)
            .encrypt_block(1, encryptor::Context::AES_GCM(flags), source, &enc_key)
            .map_err(|(_, e)| e)
            .expect("encrypt the payload (and so its covering BIB)")
            .rebuild()
            .expect("rebuild the encrypted bundle"),
    ))
    .unwrap();

    let bpa = Bpa::builder()
        .node_ids(
            hardy_bpa::node_ids::NodeIds::try_from([NodeId::Ipn(node(1))].as_slice()).unwrap(),
        )
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
            &[NodeId::Ipn(node(2))],
        )
        .await
        .unwrap();

    assert_eq!(
        ingress
            .sink
            .get()
            .unwrap()
            .dispatch(None, None, &mut received.data.clone())
            .await
            .unwrap(),
        cla::Acceptance::Accepted
    );

    // The timeout only bounds a regression: a relay that refuses the
    // per-hop rewrite parks the bundle and never offers it to the peer.
    let Event::Forward(forwarded) = recv_event(&events_rx, 5).await else {
        panic!("Expected the relay to forward the bundle");
    };
    let out = hardy_bpv7::parse::parse(forwarded).expect("the forwarded bundle parses");

    let extension = |block_type: block::Type| {
        out.bundle
            .blocks
            .values()
            .find(|b| b.block_type == block_type)
            .expect("the per-hop block is present")
    };
    let hop_info = extension(block::Type::HopCount)
        .extract::<HopInfo>(&out.data)
        .expect("the hop count decodes")
        .expect("the hop count is resident");
    assert_eq!(hop_info.count, 2, "the relay bumps the hop count");
    let previous = extension(block::Type::PreviousNode)
        .extract::<Eid>(&out.data)
        .expect("the previous node decodes")
        .expect("the previous node is resident");
    assert_eq!(previous, Eid::from(node(1)), "the relay names itself");

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

    let keys = KeySet::new(vec![enc_key]);
    let payload = DecryptingReader::new(&out.bundle.blocks, &out.data, &out.bcbs, &keys)
        .block_data(1)
        .expect("the payload decrypts downstream")
        .expect("the payload is resident");
    assert_eq!(payload.as_ref(), b"relay me");

    // shutdown() joins the pools, so anything the relay was going to offer
    // has reached the CLA by the time it returns: nothing further arrived.
    bpa.shutdown().await;
    assert!(events_rx.is_empty());
}
