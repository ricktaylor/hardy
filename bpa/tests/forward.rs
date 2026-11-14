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
    services::{self, SendOptions},
    stream::{Receiver, Segment},
};
#[cfg(feature = "rfc9173")]
use hardy_bpa::{
    builder::BpaBuilder,
    filter::{Classifier, ClassifyContext, Verdict, pack::FilterPack, slots::MetadataDelta},
};
use hardy_bpv7::{
    block,
    eid::{Eid, IpnNodeId, NodeId},
    parse::Parsed,
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
    crc,
    creation_timestamp::CreationTimestamp,
    dtn_time::DtnTime,
    hop_info::HopInfo,
    parse::parse,
    reader::ReaderExt,
    status_report::{AdministrativeRecord, ReasonCode},
};
use hardy_cbor::encode::emit;
#[cfg(feature = "rfc9173")]
use rand::{TryRng, rngs::SysRng};
// Aliased: `time`'s duration, the arithmetic type of `OffsetDateTime`.
#[cfg(feature = "rfc9173")]
use time::{Duration as TimeDuration, OffsetDateTime};

mod common;

// ---------------------------------------------------------------------------
// Events observed by the mock CLAs
// ---------------------------------------------------------------------------

enum Event {
    // The buffering CLA assembled the whole bundle.
    Forward(Bytes),
    // The streaming CLA pulled the stream to completion.
    Streamed {
        segments: Vec<Segment>,
        total_len: u64,
    },
    // The forward attempt failed.
    Failed,
}

// ---------------------------------------------------------------------------
// Mock CLAs
// ---------------------------------------------------------------------------

// Consumes the stream segment by segment, recording every segment it pulls.
struct StreamingCla {
    sink: hardy_async::sync::spin::Once<Box<dyn cla::Sink>>,
    events_tx: flume::Sender<Event>,
    // When set, the first `forward` call fails without pulling.
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

// Buffers the stream into a contiguous bundle via `stream::buffer_stream`
// — the shape every whole-buffer CLA takes on the streamed-only door.
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

// A pre-filled segment stream. The sender is dropped on return, so a
// sequence not ending in `Final` reads as a truncated stream.
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

// The block of `block_type` in `parsed`.
fn extension(parsed: &Parsed, block_type: block::Type) -> &block::Block {
    parsed
        .bundle
        .blocks
        .values()
        .find(|b| b.block_type == block_type)
        .expect("the block is present")
}

// The EID in the Previous Node block of `parsed`, as decoded from the wire.
fn previous_node(parsed: &Parsed) -> Eid {
    extension(parsed, block::Type::PreviousNode)
        .extract::<Eid>(&parsed.data)
        .expect("the previous node decodes")
        .expect("the previous node is resident")
}

// Sends `payload` to ipn:0.2.99 via `app`, returning the destination EID.
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

// A streaming CLA receives the rebuilt bundle as segments — the rebuilt
// header region, then the stored payload block — ending in a `Final`, with
// an exact `total_len`; the bundle carries this node's Previous Node and the
// payload as stored.
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
        "the rebuild travels as segments, not one flattened buffer"
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

// A CLA that buffers via `stream::buffer_stream` still receives the whole
// bundle.
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

// A failed streamed attempt takes the established requeue path: the bundle
// returns to Waiting and a routing change re-dispatches it.
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

// The buffering path reassembles a multi-segment stream.
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

// A single-`Final` stream passes through the buffering path zero-copy.
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

// A truncated stream is an error, and no partial bundle reaches the
// transport.
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

// A stream completing with fewer bytes than the declared `total_len` is
// rejected — no short transfer reaches the transport.
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

// A stream exceeding the declared `total_len` is rejected.
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

// Builds a BPA on an allocator-1 node — its 3-element IPN EIDs change bytes
// under the legacy re-encode — with the given legacy-peer patterns, wired
// to a buffering CLA and a send-only application.
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

// A next hop matching a configured legacy-peer pattern receives the bundle
// with its `Ipn` source, destination and report-to re-encoded as
// `LegacyIpn` on the wire, and this node's Previous Node written as
// `LegacyIpn` too. The deletion-report request gives the bundle this
// node's administrative endpoint as its report-to.
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
            Some(SendOptions {
                notify_deletion: true,
                ..Default::default()
            }),
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
    // This node's administrative endpoint, ipn:1.1.0, in the 2-element form.
    let admin_endpoint = Eid::LegacyIpn {
        fqnn: IpnNodeId {
            allocator_id: 1,
            node_number: 1,
        },
        service_number: 0,
    };
    assert_eq!(
        parsed.bundle.primary.report_to, admin_endpoint,
        "the report-to must be re-encoded 2-element"
    );
    assert_eq!(
        previous_node(&parsed),
        admin_endpoint,
        "the Previous Node must be written 2-element"
    );

    assert!(events_rx.is_empty());
    bpa.shutdown().await;
}

// A next hop matching no configured pattern receives the canonical
// 3-element encoding untouched, the report-to included, and this node's
// Previous Node in it. The deletion-report request gives the bundle this
// node's administrative endpoint as its report-to.
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
            Some(SendOptions {
                notify_deletion: true,
                ..Default::default()
            }),
        )
        .await
        .unwrap();

    let Event::Forward(data) = recv_event(&events_rx, 5).await else {
        panic!("Expected a forwarded bundle");
    };
    let parsed = hardy_bpv7::parse::parse(data).expect("Failed to parse forwarded bundle");
    assert_eq!(parsed.bundle.primary.id.source, source_eid);
    assert_eq!(parsed.bundle.primary.destination, dest);
    // This node's administrative endpoint, ipn:1.1.0, in the 3-element form.
    let admin_endpoint = Eid::Ipn {
        fqnn: IpnNodeId {
            allocator_id: 1,
            node_number: 1,
        },
        service_number: 0,
    };
    assert_eq!(
        parsed.bundle.primary.report_to, admin_endpoint,
        "the report-to must keep the 3-element encoding"
    );
    assert_eq!(
        previous_node(&parsed),
        admin_endpoint,
        "the Previous Node must keep the 3-element encoding"
    );

    assert!(events_rx.is_empty());
    bpa.shutdown().await;
}

// A relay on allocator 1 whose peers, ipn:1.2 and the default-allocator
// ipn:2, need legacy IPN encoding, with status reports on so that a drop's
// reason reaches the peer.
#[cfg(feature = "rfc9173")]
async fn legacy_relay() -> (Bpa, Arc<BufferedCla>, flume::Receiver<Event>) {
    legacy_relay_holding(Vec::new()).await
}

// As `legacy_relay`, holding `keys`.
#[cfg(feature = "rfc9173")]
async fn legacy_relay_holding(keys: Vec<Key>) -> (Bpa, Arc<BufferedCla>, flume::Receiver<Event>) {
    let bpa = Bpa::builder()
        .node_ids(
            hardy_bpa::node_ids::NodeIds::try_from([NodeId::Ipn(relay_node(1))].as_slice())
                .unwrap(),
        )
        .ipn_legacy_peers(vec![
            "ipn:1.2.*".parse().unwrap(),
            "ipn:2.*".parse().unwrap(),
        ])
        .key_provider(Arc::new(FixedKeys(keys)))
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
    cla.sink
        .get()
        .unwrap()
        .add_peer(
            cla::ClaAddress::Private("default-allocator peer".as_bytes().into()),
            &[remote_node(2)],
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

// The bundle itself, not a deletion report, reaches the peer, and leaves
// the node once.
#[cfg(feature = "rfc9173")]
async fn assert_forwarded(
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

// As assert_forwarded, the bundle re-encoded for the peer: its source,
// destination, report-to and Previous Node in the two-element legacy form.
#[cfg(feature = "rfc9173")]
async fn assert_forwarded_legacy(
    bpa: Bpa,
    events_rx: flume::Receiver<Event>,
    received: &Parsed,
) -> Parsed {
    let forwarded = assert_forwarded(bpa, events_rx, received).await;
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
    assert_eq!(
        forwarded.bundle.primary.report_to,
        legacy(&received.bundle.primary.report_to)
    );
    assert_eq!(
        previous_node(&forwarded),
        Eid::LegacyIpn {
            fqnn: relay_node(1),
            service_number: 0,
        },
        "the relay names itself in the two-element form"
    );
    forwarded
}

// Sign `targets` of `bundle` under `scope` from its source: one BIB where
// the scope lets the targets share it, one each otherwise.
#[cfg(feature = "rfc9173")]
fn signed(bundle: &Parsed, targets: &[u64], scope: ScopeFlags, key: &Key) -> Parsed {
    let mut signing = Signer::new(&bundle.bundle, &bundle.data);
    for &target in targets {
        signing
            .sign_block(
                target,
                signer::Context::HMAC_SHA2(scope.clone()),
                bundle.bundle.primary.id.source.clone(),
                key,
            )
            .expect("sign the target");
    }
    hardy_bpv7::parse::parse(Bytes::from(
        signing.rebuild().expect("rebuild the signed bundle"),
    ))
    .unwrap()
}

// A legacy next hop needs a re-encoded primary block, and a BIB signs the
// primary, so the signature could not survive: the bundle is dropped with
// `UnexpectedSecurityOperation`. The one BIB also covers the Hop Count,
// which the relay, without the key, leaves as received: the primary stays
// signed regardless. The scope leaves the security header out, so the two
// targets share the BIB.
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

// No BIB targets the primary, but the payload's BIB has the primary in its
// scope (RFC 9173's default), so the re-encode would break it: dropped.
#[cfg(feature = "rfc9173")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn legacy_peer_drops_a_bundle_whose_bib_scopes_the_primary() {
    let (bpa, cla, events_rx) = legacy_relay().await;
    let (built, _) = legacy_bound_bundle();
    let received = signed(&built, &[1], ScopeFlags::default(), &sign_key());

    receive(&cla, &received).await;
    assert_dropped_as_unexpected(bpa, events_rx, &received).await;
}

// A default-allocator bundle's ipn EIDs are 2-element already, so its
// primary needs no re-encode, and a payload BIB whose scope includes the
// primary (RFC 9173's default) does not stand in the way: the bundle goes
// to the legacy peer with its EIDs as received, and the signature still
// verifies.
#[cfg(feature = "rfc9173")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn legacy_peer_takes_a_default_allocator_bundle_whose_bib_scopes_the_primary() {
    let (bpa, cla, events_rx) = legacy_relay().await;
    let (_, data) = Builder::new("ipn:3.1".parse().unwrap(), "ipn:2.99".parse().unwrap())
        .with_flags(BundleFlags {
            delete_report_requested: true,
            ..Default::default()
        })
        .with_report_to("ipn:2.9".parse().unwrap())
        .with_payload(b"default allocator".as_slice().into())
        .build(CreationTimestamp::now())
        .expect("build the bundle");
    let built = parse(Bytes::from(data)).unwrap();
    let key = sign_key();
    let received = signed(&built, &[1], ScopeFlags::default(), &key);

    receive(&cla, &received).await;
    let forwarded = assert_forwarded(bpa, events_rx, &received).await;
    assert_eq!(
        forwarded.bundle.primary.id.source,
        received.bundle.primary.id.source
    );
    assert_eq!(
        forwarded.bundle.primary.destination,
        received.bundle.primary.destination
    );
    assert_eq!(
        forwarded.bundle.primary.report_to,
        received.bundle.primary.report_to
    );
    common::verify_all_bibs(&forwarded.data, &KeySet::new(vec![key]))
        .expect("the payload's signature, over the primary, verifies at the peer");
}

// A payload BIB whose scope leaves the primary out survives the re-encode:
// the bundle goes to the legacy peer, and the signature still verifies.
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
    common::verify_all_bibs(&forwarded.data, &KeySet::new(vec![key]))
        .expect("the payload's signature verifies after the re-encode");
}

// A BCB whose AAD scope includes the primary (RFC 9173's default) counts
// the same way: dropped.
#[cfg(feature = "rfc9173")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn legacy_peer_drops_a_bundle_whose_bcb_scopes_the_primary() {
    let (bpa, cla, events_rx) = legacy_relay().await;
    let (built, _) = legacy_bound_bundle();
    let key = enc_key();
    let mut encryptor = Encryptor::new(&built.bundle, &built.data);
    encryptor
        .encrypt_block(
            1,
            encryptor::Context::AES_GCM(ScopeFlags::default()),
            built.bundle.primary.id.source.clone(),
            &key,
        )
        .expect("encrypt the payload");
    let received = hardy_bpv7::parse::parse(Bytes::from(
        encryptor.rebuild().expect("rebuild the encrypted bundle"),
    ))
    .unwrap();

    receive(&cla, &received).await;
    assert_dropped_as_unexpected(bpa, events_rx, &received).await;
}

// A BCB whose AAD scope leaves the primary out survives the re-encode: the
// bundle goes to the legacy peer, and its payload still decrypts.
#[cfg(feature = "rfc9173")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn legacy_peer_takes_a_bundle_whose_bcb_leaves_the_primary_out() {
    let (bpa, cla, events_rx) = legacy_relay().await;
    let (built, _) = legacy_bound_bundle();
    let key = enc_key();
    let mut encryptor = Encryptor::new(&built.bundle, &built.data);
    encryptor
        .encrypt_block(
            1,
            encryptor::Context::AES_GCM(ScopeFlags {
                include_primary_block: false,
                ..ScopeFlags::default()
            }),
            built.bundle.primary.id.source.clone(),
            &key,
        )
        .expect("encrypt the payload");
    let received = hardy_bpv7::parse::parse(Bytes::from(
        encryptor.rebuild().expect("rebuild the encrypted bundle"),
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

// A default-scope BIB over the Hop Count alone, whose key the relay holds:
// ingress accepts the operation and strips the emptied BIB, so nothing is
// left for the re-encode to break, and the bundle goes to the peer. Without
// the key the relay is not the operation's acceptor, so the BIB stays, its
// scope covering the primary, and the bundle is dropped.
#[cfg(feature = "rfc9173")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn legacy_peer_takes_a_bundle_whose_only_bib_ingress_removes() {
    let sign_key = sign_key();
    let (built, hop_count) = legacy_bound_bundle();
    let received = signed(&built, &[hop_count], ScopeFlags::default(), &sign_key);

    let (bpa, cla, events_rx) = legacy_relay_holding(vec![sign_key]).await;
    receive(&cla, &received).await;
    let forwarded = assert_forwarded_legacy(bpa, events_rx, &received).await;
    assert!(
        !forwarded
            .bundle
            .blocks
            .values()
            .any(|b| b.block_type == block::Type::BlockIntegrity),
        "the emptied BIB went at ingress"
    );

    let (bpa, cla, events_rx) = legacy_relay().await;
    receive(&cla, &received).await;
    assert_dropped_as_unexpected(bpa, events_rx, &received).await;
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
// PreviousNode and bumped the hop count. Each of `encrypted`, a per-hop block
// type with a security source and a key, has that block BCB-encrypted as
// well.
#[cfg(feature = "rfc9173")]
fn protected_bundle(
    sign_key: &Key,
    enc_key: &Key,
    encrypted: &[(block::Type, &Eid, &Key)],
) -> Parsed {
    let source: Eid = "ipn:1.3.1".parse().unwrap();
    let (_, data) = Builder::new(source.clone(), "ipn:1.2.99".parse().unwrap())
        .with_hop_count(&HopInfo {
            limit: NonZeroU8::new(64).unwrap(),
            count: 1,
        })
        .add_extension_block(block::Type::PreviousNode)
        .expect("add the PreviousNode block")
        .build(emit(&Eid::from(relay_node(4))).0.into())
        .with_payload(b"relay me".as_slice().into())
        .build(CreationTimestamp::now())
        .expect("build the bundle");
    let built = hardy_bpv7::parse::parse(Bytes::from(data)).unwrap();
    let mut signer = Signer::new(&built.bundle, &built.data);
    signer
        .sign_block(
            1,
            signer::Context::HMAC_SHA2(ScopeFlags::default()),
            source.clone(),
            sign_key,
        )
        .expect("sign the payload");
    let signed = hardy_bpv7::parse::parse(Bytes::from(
        signer.rebuild().expect("rebuild the signed bundle"),
    ))
    .unwrap();
    let flags = ScopeFlags {
        include_security_header: false,
        ..ScopeFlags::default()
    };
    let mut encryptor = Encryptor::new(&signed.bundle, &signed.data);
    encryptor
        .encrypt_block(
            1,
            encryptor::Context::AES_GCM(flags.clone()),
            source.clone(),
            enc_key,
        )
        .expect("encrypt the payload (and so its covering BIB)");
    for &(block_type, security_source, key) in encrypted {
        let number = *signed
            .bundle
            .blocks
            .iter()
            .find(|(_, b)| b.block_type == block_type)
            .expect("the bundle carries the per-hop block")
            .0;
        encryptor
            .encrypt_block(
                number,
                encryptor::Context::AES_GCM(flags.clone()),
                security_source.clone(),
                key,
            )
            .expect("encrypt the per-hop block");
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

// A key provider lending each key for its one security source only, so a
// relay can hold a hop's key without the bundle source's.
#[cfg(feature = "rfc9173")]
#[derive(Clone)]
struct KeysBySource(Vec<(Eid, Key)>);

#[cfg(feature = "rfc9173")]
impl KeyProvider for KeysBySource {
    fn key_source(&self, _bundle: &hardy_bpv7::Bundle, _data: &[u8]) -> Box<dyn KeySource> {
        Box::new(self.clone())
    }
}

#[cfg(feature = "rfc9173")]
impl KeySource for KeysBySource {
    fn key<'a>(&'a self, source: &Eid, operations: &[Operation]) -> Option<&'a Key> {
        self.0
            .iter()
            .find(|(s, k)| {
                s == source
                    && k.operations
                        .as_ref()
                        .is_some_and(|ops| operations.iter().any(|op| ops.contains(op)))
            })
            .map(|(_, k)| k)
    }
}

// Relays `received` through node 1.1 holding `keys` (none for a keyless
// relay) toward its peer node 1.2, returning the forwarded bundle.
#[cfg(feature = "rfc9173")]
async fn relay(received: &Bytes, keys: Vec<Key>) -> Parsed {
    relay_with(received, keys, |builder| builder).await
}

// As `relay`, with the relay's builder configured by `configure`.
#[cfg(feature = "rfc9173")]
async fn relay_with(
    received: &Bytes,
    keys: Vec<Key>,
    configure: impl FnOnce(BpaBuilder) -> BpaBuilder,
) -> Parsed {
    let bpa = configure(
        Bpa::builder()
            .node_ids(
                hardy_bpa::node_ids::NodeIds::try_from([NodeId::Ipn(relay_node(1))].as_slice())
                    .unwrap(),
            )
            .key_provider(Arc::new(FixedKeys(keys))),
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
    let received = protected_bundle(&sign_key(), &enc_key, &[]);

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
    let source: Eid = "ipn:1.3.1".parse().unwrap();
    let received = protected_bundle(
        &sign_key(),
        &enc_key,
        &[(block::Type::HopCount, &source, &enc_key)],
    );
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

// The Hop Count, decrypted downstream with `key`.
#[cfg(feature = "rfc9173")]
fn hop_count_under(out: &Parsed, key: Key) -> HopInfo {
    let hop_count = out
        .bundle
        .blocks
        .iter()
        .find(|(_, b)| b.block_type == block::Type::HopCount)
        .map(|(n, _)| *n)
        .expect("the Hop Count block is present");
    let keys = KeySet::new(vec![key]);
    DecryptingReader::new(&out.bundle.blocks, &out.data, &out.bcbs, &keys)
        .extract::<HopInfo>(hop_count)
        .expect("the hop count decodes")
        .expect("the hop count decrypts downstream")
}

// A relay holding the key to an encrypted Hop Count accepts its BCB at
// ingress: the encrypted BIB it decrypts there covers only the payload, so
// the block is written in plaintext, and the relay increments it. The
// payload, its BIB and their BCB travel untouched.
#[cfg(feature = "rfc9173")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn key_holding_relay_increments_an_encrypted_hop_count() {
    let enc_key = enc_key();
    let source: Eid = "ipn:1.3.1".parse().unwrap();
    let received = protected_bundle(
        &sign_key(),
        &enc_key,
        &[(block::Type::HopCount, &source, &enc_key)],
    );

    let out = relay(&received.data, vec![enc_key.clone()]).await;

    let hop_count = extension(&out, block::Type::HopCount);
    assert!(hop_count.bcb.is_none(), "the Hop Count leaves in plaintext");
    let hop_info = hop_count
        .extract::<HopInfo>(&out.data)
        .expect("the hop count decodes")
        .expect("the hop count is resident");
    assert_eq!(hop_info.count, 2, "the relay increments the count");
    assert_eq!(
        extension(&out, block::Type::Payload).payload(&out.data),
        extension(&received, block::Type::Payload).payload(&received.data),
        "the payload's ciphertext is untouched"
    );
    let keys = KeySet::new(vec![enc_key]);
    let payload = DecryptingReader::new(&out.bundle.blocks, &out.data, &out.bcbs, &keys)
        .block_data(1)
        .expect("the payload decrypts downstream")
        .expect("the payload is resident");
    assert_eq!(payload.as_ref(), b"relay me");
}

// A relay holding the key to an encrypted Hop Count, but not to the bundle
// source's encrypted BIB, cannot tell whether that BIB covers the block, so
// it cannot accept the block's operations. The increment is a SHOULD, so
// the block travels unchanged, still counting one hop, rather than holding
// the bundle back.
#[cfg(feature = "rfc9173")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn key_holding_relay_forwards_a_possibly_signed_hop_count_unchanged() {
    let previous = Eid::from(relay_node(4));
    let hop_key = enc_key();
    let received = protected_bundle(
        &sign_key(),
        &enc_key(),
        &[(block::Type::HopCount, &previous, &hop_key)],
    );

    let out = relay_with(&received.data, Vec::new(), |builder| {
        builder.key_provider(Arc::new(KeysBySource(vec![(
            previous.clone(),
            hop_key.clone(),
        )])))
    })
    .await;

    assert_protected_blocks_untouched(&received, &out);
    assert_eq!(
        hop_count_under(&out, hop_key).count,
        1,
        "the count is not incremented"
    );
}

// The forwarded Previous Node, in plaintext, and the payload's ciphertext
// unchanged from `received`'s.
#[cfg(feature = "rfc9173")]
fn assert_previous_node_replaced(received: &Parsed, out: &Parsed) {
    let previous = extension(out, block::Type::PreviousNode);
    assert!(
        previous.bcb.is_none(),
        "the Previous Node leaves in plaintext"
    );
    assert_eq!(
        previous
            .extract::<Eid>(&out.data)
            .expect("the previous node decodes")
            .expect("the previous node is resident"),
        Eid::from(relay_node(1)),
        "the relay names itself"
    );
    assert_eq!(
        extension(out, block::Type::Payload).payload(&out.data),
        extension(received, block::Type::Payload).payload(&received.data),
        "the payload's ciphertext is untouched"
    );
}

// The common relay case: the previous node encrypted its Previous Node for
// this hop, and the bundle carries its source's end-to-end encrypted BIB,
// whose key the relay does not hold. No valid bundle has an encrypted BIB
// over a Previous Node, so the relay, holding the hop's key, accepts the
// block's BCB and forwards the bundle with its own Previous Node.
#[cfg(feature = "rfc9173")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn key_holding_relay_accepts_an_encrypted_previous_node_beside_an_unreadable_bib() {
    let previous = Eid::from(relay_node(4));
    let hop_key = enc_key();
    let received = protected_bundle(
        &sign_key(),
        &enc_key(),
        &[(block::Type::PreviousNode, &previous, &hop_key)],
    );

    let out = relay_with(&received.data, Vec::new(), |builder| {
        builder.key_provider(Arc::new(KeysBySource(vec![(
            previous.clone(),
            hop_key.clone(),
        )])))
    })
    .await;

    assert_previous_node_replaced(&received, &out);
}

// As above, at a relay without the hop's key: it fails to accept the
// Previous Node's BCB, so it discards the block with that BCB, and forwards
// the bundle with its own Previous Node rather than holding it back.
#[cfg(feature = "rfc9173")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn keyless_relay_replaces_an_encrypted_previous_node_beside_an_unreadable_bib() {
    let previous = Eid::from(relay_node(4));
    let received = protected_bundle(
        &sign_key(),
        &enc_key(),
        &[(block::Type::PreviousNode, &previous, &enc_key())],
    );

    let out = relay(&received.data, Vec::new()).await;

    assert_previous_node_replaced(&received, &out);
}

// A relay holding every key, to a Hop Count its source signed with another
// block, the payload or an extension block, and then encrypted with it, so
// that one encrypted BIB covers both: removing the BCB operation on the Hop
// Count takes those on that BIB and on the other block with it, all or none
// (RFC 9172 §3.9), and the other block is not the relay's to accept. The
// Hop Count travels unchanged, still counting one hop, and the other block
// keeps its ciphertext.
#[cfg(feature = "rfc9173")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn key_holding_relay_forwards_a_hop_count_sharing_an_encrypted_bib_unchanged() {
    for partner in [block::Type::Payload, block::Type::Unrecognised(199)] {
        let (sign_key, enc_key) = (sign_key(), enc_key());
        let source: Eid = "ipn:1.3.1".parse().unwrap();
        let (_, data) = Builder::new(source.clone(), "ipn:1.2.99".parse().unwrap())
            .with_hop_count(&HopInfo {
                limit: NonZeroU8::new(64).unwrap(),
                count: 1,
            })
            .add_extension_block(block::Type::Unrecognised(199))
            .expect("add the extension block")
            .build(b"partner".as_slice().into())
            .with_payload(b"relay me".as_slice().into())
            .build(CreationTimestamp::now())
            .expect("build the bundle");
        let built = hardy_bpv7::parse::parse(Bytes::from(data)).unwrap();
        let number = |block_type| {
            *built
                .bundle
                .blocks
                .iter()
                .find(|(_, b)| b.block_type == block_type)
                .expect("the bundle carries the block")
                .0
        };
        let targets = [number(partner), number(block::Type::HopCount)];
        // Without the security header in scope, the two results share one BIB.
        let flags = ScopeFlags {
            include_security_header: false,
            ..ScopeFlags::default()
        };
        let mut signing = Signer::new(&built.bundle, &built.data);
        for target in targets {
            signing
                .sign_block(
                    target,
                    signer::Context::HMAC_SHA2(flags.clone()),
                    source.clone(),
                    &sign_key,
                )
                .expect("sign the target");
        }
        let signed = hardy_bpv7::parse::parse(Bytes::from(
            signing.rebuild().expect("rebuild the signed bundle"),
        ))
        .unwrap();
        let mut encrypting = Encryptor::new(&signed.bundle, &signed.data);
        for target in targets {
            encrypting
                .encrypt_block(
                    target,
                    encryptor::Context::AES_GCM(flags.clone()),
                    source.clone(),
                    &enc_key,
                )
                .expect("encrypt the target");
        }
        let received = hardy_bpv7::parse::parse(Bytes::from(
            encrypting.rebuild().expect("rebuild the encrypted bundle"),
        ))
        .unwrap();
        let bibs: Vec<&block::Block> = received
            .bundle
            .blocks
            .values()
            .filter(|b| b.block_type == block::Type::BlockIntegrity)
            .collect();
        assert!(
            bibs.len() == 1 && bibs[0].bcb.is_some(),
            "{partner:?}: one encrypted BIB covers both blocks"
        );

        // Without the signing key the Hop Count's operation goes unchecked,
        // which is no failure either.
        for keys in [
            vec![sign_key.clone(), enc_key.clone()],
            vec![enc_key.clone()],
        ] {
            let out = relay(&received.data, keys).await;

            assert_protected_blocks_untouched(&received, &out);
            assert_eq!(
                hop_count_under(&out, enc_key.clone()).count,
                1,
                "{partner:?}: the count is not incremented"
            );
        }
    }
}

// A bundle from ipn:1.3.1, created at `timestamp` and asking for deletion
// reports at ipn:1.2.9 (behind the relay's peer), whose `block_type` block
// (Previous Node or Bundle Age), flagged `flags`, its source signed then
// encrypted, so the BIB over it is encrypted too. With `encrypt_payload` the
// payload is encrypted as well, and only a key holder can see what the
// encrypted BIB covers.
#[cfg(feature = "rfc9173")]
fn per_hop_under_encrypted_bib(
    block_type: block::Type,
    flags: block::Flags,
    timestamp: CreationTimestamp,
    encrypt_payload: bool,
    sign_key: &Key,
    enc_key: &Key,
) -> Parsed {
    let source: Eid = "ipn:1.3.1".parse().unwrap();
    let body = if block_type == block::Type::PreviousNode {
        emit(&Eid::from(relay_node(4))).0
    } else {
        emit(&5_000u64).0
    };
    let (_, data) = Builder::new(source.clone(), "ipn:1.2.99".parse().unwrap())
        .with_flags(BundleFlags {
            delete_report_requested: true,
            ..Default::default()
        })
        .with_report_to("ipn:1.2.9".parse().unwrap())
        .add_extension_block(block_type)
        .expect("add the per-hop block")
        .with_flags(flags)
        .build(body.into())
        .with_payload(b"relay me".as_slice().into())
        .build(timestamp)
        .expect("build the bundle");
    let built = hardy_bpv7::parse::parse(Bytes::from(data)).unwrap();
    let target = *built
        .bundle
        .blocks
        .iter()
        .find(|(_, b)| b.block_type == block_type)
        .expect("the per-hop block is present")
        .0;
    let mut signer = Signer::new(&built.bundle, &built.data);
    signer
        .sign_block(
            target,
            signer::Context::HMAC_SHA2(ScopeFlags::default()),
            source.clone(),
            sign_key,
        )
        .expect("sign the per-hop block");
    let signed = hardy_bpv7::parse::parse(Bytes::from(
        signer.rebuild().expect("rebuild the signed bundle"),
    ))
    .unwrap();
    let flags = ScopeFlags {
        include_security_header: false,
        ..ScopeFlags::default()
    };
    let mut encrypting = Encryptor::new(&signed.bundle, &signed.data);
    encrypting
        .encrypt_block(
            target,
            encryptor::Context::AES_GCM(flags.clone()),
            source.clone(),
            enc_key,
        )
        .expect("encrypt the per-hop block, and so its BIB");
    if encrypt_payload {
        encrypting
            .encrypt_block(1, encryptor::Context::AES_GCM(flags), source, enc_key)
            .expect("encrypt the payload");
    }
    hardy_bpv7::parse::parse(Bytes::from(
        encrypting.rebuild().expect("rebuild the encrypted bundle"),
    ))
    .unwrap()
}

// Hand `received` to a relay holding `keys`, and return the reason of the
// deletion report it sends in the bundle's place.
#[cfg(feature = "rfc9173")]
async fn relay_refusal(received: &Parsed, keys: Vec<Key>) -> ReasonCode {
    let bpa = Bpa::builder()
        .node_ids(
            hardy_bpa::node_ids::NodeIds::try_from([NodeId::Ipn(relay_node(1))].as_slice())
                .unwrap(),
        )
        .key_provider(Arc::new(FixedKeys(keys)))
        .status_reports(true)
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
            .dispatch(None, None, &mut received.data.clone())
            .await
            .unwrap(),
        cla::Acceptance::Accepted
    );

    // The timeout only bounds a regression.
    let Event::Forward(report) = recv_event(&events_rx, 5).await else {
        panic!("Expected the deletion report");
    };
    let report = hardy_bpv7::parse::parse(report).expect("the report parses");
    assert!(
        report.bundle.primary.flags.is_admin_record,
        "only a status report leaves the relay"
    );
    let AdministrativeRecord::BundleStatusReport(report) = report.bundle.blocks[&1]
        .extract::<AdministrativeRecord>(&report.data)
        .expect("the payload is an administrative record")
        .expect("the report payload is resident");
    assert_eq!(report.bundle_id, received.bundle.primary.id);
    assert!(report.deleted.is_some(), "the report asserts the deletion");

    // shutdown() joins the pools: the bundle itself never left the node.
    bpa.shutdown().await;
    assert!(events_rx.is_empty());
    report.reason
}

// No block of `out` is BCB-covered or a security block.
#[cfg(feature = "rfc9173")]
fn unprotected(out: &Parsed) -> bool {
    out.bundle.blocks.values().all(|b| {
        b.bcb.is_none()
            && !matches!(
                b.block_type,
                block::Type::BlockIntegrity | block::Type::BlockSecurity
            )
    })
}

// An encrypted BIB over a Bundle Age block is a combination no valid bundle
// carries, and with that block the only encrypted one besides the BIB, a
// keyless relay can tell from the structure that the BIB may cover it. On a
// bundle with a creation time the relay replaces a Bundle Age it cannot
// accept from the clock, no failure whatever the block's flags: the received
// block goes, the BIB whole with it and the BCB they empty, and the bundle
// is forwarded with a fresh Bundle Age in plaintext.
#[cfg(feature = "rfc9173")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn keyless_relay_replaces_a_bundle_age_an_encrypted_bib_may_cover() {
    let created = DtnTime::saturating_from(OffsetDateTime::now_utc() - TimeDuration::seconds(10));
    let received = per_hop_under_encrypted_bib(
        block::Type::BundleAge,
        block::Flags {
            delete_bundle_on_failure: true,
            report_on_failure: true,
            ..Default::default()
        },
        CreationTimestamp::from_parts(Some(created), 1),
        false,
        &sign_key(),
        &enc_key(),
    );

    let out = relay(&received.data, Vec::new()).await;

    assert!(unprotected(&out), "the BIB and BCB go with the block");
    let age = extension(&out, block::Type::BundleAge)
        .extract::<u64>(&out.data)
        .expect("the bundle age decodes")
        .expect("the bundle age is resident");
    assert!(
        age >= 10_000,
        "the age is the clock's, not the discarded block's: {age} ms"
    );
}

// The same structure over a Previous Node block, which the relay must
// replace and cannot accept without the BIB's key: a failed acceptance, so
// the block's flags decide. Unflagged, the block is discarded with the BIB,
// whole, and the relay forwards the bundle with its own Previous Node;
// flagged to delete the bundle, the bundle is deleted as one the relay
// cannot process, "Block unsupported".
#[cfg(feature = "rfc9173")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn keyless_relay_replaces_a_previous_node_an_encrypted_bib_may_cover() {
    let received = per_hop_under_encrypted_bib(
        block::Type::PreviousNode,
        block::Flags::default(),
        CreationTimestamp::now(),
        false,
        &sign_key(),
        &enc_key(),
    );
    let out = relay(&received.data, Vec::new()).await;
    assert!(unprotected(&out), "the BIB and BCB go with the block");
    assert_previous_node_replaced(&received, &out);

    let received = per_hop_under_encrypted_bib(
        block::Type::PreviousNode,
        block::Flags {
            delete_bundle_on_failure: true,
            ..Default::default()
        },
        CreationTimestamp::now(),
        false,
        &sign_key(),
        &enc_key(),
    );
    assert_eq!(
        relay_refusal(&received, Vec::new()).await,
        ReasonCode::BlockUnsupported
    );
}

// With the payload encrypted too, the structure proves nothing, and a relay
// holding the key reads no BIB on a Previous Node's account: no valid bundle
// has an encrypted BIB over one, so the block reads as covered by none, as a
// keyless relay reads it. The relay accepts the block's BCB operation,
// whatever the block's flags, and forwards the bundle with its own Previous
// Node, the encrypted BIB travelling as received for an acceptance under the
// source's BCB to read, and the payload keeping its ciphertext.
#[cfg(feature = "rfc9173")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn key_holding_relay_reads_no_bib_on_a_previous_nodes_account() {
    let enc_key = enc_key();
    for flags in [
        block::Flags::default(),
        block::Flags {
            delete_bundle_on_failure: true,
            ..Default::default()
        },
    ] {
        let received = per_hop_under_encrypted_bib(
            block::Type::PreviousNode,
            flags,
            CreationTimestamp::now(),
            true,
            &sign_key(),
            &enc_key,
        );
        let out = relay(&received.data, vec![enc_key.clone()]).await;
        assert_previous_node_replaced(&received, &out);
        assert!(
            out.bundle
                .blocks
                .values()
                .any(|b| b.block_type == block::Type::BlockIntegrity && b.bcb.is_some()),
            "the encrypted BIB travels as received"
        );
        assert!(
            extension(&out, block::Type::Payload).bcb.is_some(),
            "the payload keeps its BCB operation"
        );
    }
}

// A bundle from ipn:1.3.1 whose Previous Node its source signed with the
// payload into one BIB and then encrypted, so the BIB is encrypted too, and
// with it the payload, which the encryptor takes along rather than split the
// BIB: an encrypted BIB over a Previous Node and a block the relay does not
// accept, each under a BCB of its own.
#[cfg(feature = "rfc9173")]
fn previous_node_signed_with_the_payload(sign_key: &Key, enc_key: &Key) -> Parsed {
    let source: Eid = "ipn:1.3.1".parse().unwrap();
    let (_, data) = Builder::new(source.clone(), "ipn:1.2.99".parse().unwrap())
        .add_extension_block(block::Type::PreviousNode)
        .expect("add the Previous Node block")
        .build(emit(&Eid::from(relay_node(4))).0.into())
        .with_payload(b"relay me".as_slice().into())
        .build(CreationTimestamp::now())
        .expect("build the bundle");
    let built = hardy_bpv7::parse::parse(Bytes::from(data)).unwrap();
    let previous = *built
        .bundle
        .blocks
        .iter()
        .find(|(_, b)| b.block_type == block::Type::PreviousNode)
        .expect("the Previous Node block is present")
        .0;
    // Without the security header in scope, the two results share one BIB.
    let flags = ScopeFlags {
        include_security_header: false,
        ..ScopeFlags::default()
    };
    let mut signing = Signer::new(&built.bundle, &built.data);
    for target in [previous, 1] {
        signing
            .sign_block(
                target,
                signer::Context::HMAC_SHA2(flags.clone()),
                source.clone(),
                sign_key,
            )
            .expect("sign the target");
    }
    let signed = hardy_bpv7::parse::parse(Bytes::from(
        signing.rebuild().expect("rebuild the signed bundle"),
    ))
    .unwrap();
    let mut encryptor = Encryptor::new(&signed.bundle, &signed.data);
    encryptor
        .encrypt_block(
            previous,
            encryptor::Context::AES_GCM(flags),
            source,
            enc_key,
        )
        .expect("encrypt the Previous Node, and so its BIB");
    hardy_bpv7::parse::parse(Bytes::from(
        encryptor.rebuild().expect("rebuild the encrypted bundle"),
    ))
    .unwrap()
}

// A relay holding every key reads no BIB on the Previous Node's account
// either, and with the payload encrypted too the structure proves nothing:
// the block reads as covered by none, its BCB operation is accepted, and the
// relay forwards the bundle with its own Previous Node, the encrypted BIB
// and the payload travelling as received under their BCBs.
#[cfg(feature = "rfc9173")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_key_holding_relay_reads_no_bib_on_a_previous_nodes_account() {
    let (sign_key, enc_key) = (sign_key(), enc_key());
    let received = previous_node_signed_with_the_payload(&sign_key, &enc_key);

    let out = relay(&received.data, vec![sign_key, enc_key]).await;

    assert_previous_node_replaced(&received, &out);
    let count = |block_type| {
        out.bundle
            .blocks
            .values()
            .filter(|b| b.block_type == block_type)
            .count()
    };
    assert_eq!(
        count(block::Type::BlockIntegrity),
        1,
        "the BIB travels as received"
    );
    assert_eq!(
        count(block::Type::BlockSecurity),
        2,
        "the BIB's BCB and the payload's stay; the Previous Node's goes"
    );
}

// A bundle from ipn:1.3.1 counting one hop, whose Hop Count, carrying no
// CRC, its source signed with the payload into one BIB, then altered, so
// the operation on it fails, and encrypted with the payload under one BCB,
// which takes the BIB with them: one encrypted BIB over the Hop Count and
// the payload.
#[cfg(feature = "rfc9173")]
fn with_failed_hop_count_sharing_an_encrypted_bib(sign_key: &Key, enc_key: &Key) -> Parsed {
    let source: Eid = "ipn:1.3.1".parse().unwrap();
    let (_, data) = Builder::new(source.clone(), "ipn:1.2.99".parse().unwrap())
        // The Hop Count carries no CRC, as RFC 9173 §3.8.1 and §4.8.1 leave a
        // target, so that an alteration of its data reaches the security
        // check rather than the CRC.
        .add_extension_block(block::Type::HopCount)
        .unwrap()
        .with_flags(block::Flags {
            report_on_failure: true,
            must_replicate: true,
            ..Default::default()
        })
        .with_crc_type(crc::CrcType::None)
        .build(
            emit(&HopInfo {
                limit: NonZeroU8::new(64).unwrap(),
                count: 1,
            })
            .0
            .into(),
        )
        .with_payload(b"relay me".as_slice().into())
        .build(CreationTimestamp::now())
        .expect("build the bundle");
    let built = hardy_bpv7::parse::parse(Bytes::from(data)).unwrap();
    let hop_count = *built
        .bundle
        .blocks
        .iter()
        .find(|(_, b)| b.block_type == block::Type::HopCount)
        .expect("the Hop Count block is present")
        .0;
    let targets = [hop_count, 1];
    // Without the security header in scope, the two results share one BIB.
    let flags = ScopeFlags {
        include_security_header: false,
        ..ScopeFlags::default()
    };
    let mut signing = Signer::new(&built.bundle, &built.data);
    for target in targets {
        signing
            .sign_block(
                target,
                signer::Context::HMAC_SHA2(flags.clone()),
                source.clone(),
                sign_key,
            )
            .expect("sign the target");
    }
    let signed = hardy_bpv7::parse::parse(Bytes::from(
        signing.rebuild().expect("rebuild the signed bundle"),
    ))
    .unwrap();
    // Raise the data's last byte, the count of 1, by one: still valid CBOR,
    // no longer what was signed. The block carries no CRC, so the altered
    // block still parses.
    let end = usize::try_from(signed.bundle.blocks[&hop_count].payload_range().end).unwrap();
    let mut data = signed.data.to_vec();
    data[end - 1] += 1;
    let altered = hardy_bpv7::parse::parse(Bytes::from(data)).unwrap();
    let mut encrypting = Encryptor::new(&altered.bundle, &altered.data);
    for target in targets {
        encrypting
            .encrypt_block(
                target,
                encryptor::Context::AES_GCM(flags.clone()),
                source.clone(),
                enc_key,
            )
            .expect("encrypt the target");
    }
    hardy_bpv7::parse::parse(Bytes::from(
        encrypting.rebuild().expect("rebuild the encrypted bundle"),
    ))
    .unwrap()
}

// A relay holding every key is the acceptor of the operations on the Hop
// Count, and finds its signature failed: the block is discarded (RFC 9172
// §5.1.2) and, a per-hop block, takes the encrypted BIB it shares with the
// payload with it, whole, rather than staying as received to keep that BIB
// intact. The payload keeps its ciphertext, without the BIB's operation.
#[cfg(feature = "rfc9173")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn key_holding_relay_discards_a_failed_hop_count_with_its_shared_encrypted_bib() {
    let (sign_key, enc_key) = (sign_key(), enc_key());
    let received = with_failed_hop_count_sharing_an_encrypted_bib(&sign_key, &enc_key);

    let out = relay(&received.data, vec![sign_key, enc_key]).await;

    let present = |block_type| {
        out.bundle
            .blocks
            .values()
            .any(|b| b.block_type == block_type)
    };
    assert!(!present(block::Type::HopCount), "the failed Hop Count goes");
    assert!(
        !present(block::Type::BlockIntegrity),
        "the encrypted BIB goes whole"
    );
    assert_eq!(
        extension(&out, block::Type::Payload).payload(&out.data),
        extension(&received, block::Type::Payload).payload(&received.data),
        "the payload's ciphertext is untouched"
    );
}

// A bundle from ipn:1.3.1 counting one hop, whose Hop Count its source
// signed under `sign_key`, in plaintext.
#[cfg(feature = "rfc9173")]
fn with_signed_hop_count(sign_key: &Key) -> Parsed {
    let source: Eid = "ipn:1.3.1".parse().unwrap();
    let (_, data) = Builder::new(source.clone(), "ipn:1.2.99".parse().unwrap())
        .with_hop_count(&HopInfo {
            limit: NonZeroU8::new(64).unwrap(),
            count: 1,
        })
        .with_payload(b"relay me".as_slice().into())
        .build(CreationTimestamp::now())
        .expect("build the bundle");
    let built = hardy_bpv7::parse::parse(Bytes::from(data)).unwrap();
    let hop_count = *built
        .bundle
        .blocks
        .iter()
        .find(|(_, b)| b.block_type == block::Type::HopCount)
        .expect("the Hop Count block is present")
        .0;
    let mut signer = Signer::new(&built.bundle, &built.data);
    signer
        .sign_block(
            hop_count,
            signer::Context::HMAC_SHA2(ScopeFlags::default()),
            source,
            sign_key,
        )
        .expect("sign the Hop Count");
    hardy_bpv7::parse::parse(Bytes::from(
        signer.rebuild().expect("rebuild the signed bundle"),
    ))
    .unwrap()
}

// A relay is the acceptor of the operations on a Hop Count only where it
// holds the key to check them: holding it, it accepts the signature at
// ingress and increments the count; without it, the block is not its to
// change, so it travels unchanged with its BIB, still counting one hop.
#[cfg(feature = "rfc9173")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_relay_increments_a_signed_hop_count_only_as_its_acceptor() {
    let sign_key = sign_key();
    let received = with_signed_hop_count(&sign_key);
    let count = |out: &Parsed| {
        extension(out, block::Type::HopCount)
            .extract::<HopInfo>(&out.data)
            .expect("the hop count decodes")
            .expect("the hop count is resident")
            .count
    };

    let out = relay(&received.data, vec![sign_key]).await;
    assert_eq!(count(&out), 2, "the acceptor increments the count");
    assert!(unprotected(&out), "the BIB empties at ingress");

    let out = relay(&received.data, Vec::new()).await;
    assert_protected_blocks_untouched(&received, &out);
    assert_eq!(count(&out), 1, "a keyless relay leaves the count");
    assert_eq!(
        extension(&out, block::Type::HopCount).payload(&out.data),
        extension(&received, block::Type::HopCount).payload(&received.data),
        "the Hop Count leaves byte-identical"
    );
}

// A bundle from ipn:1.3.1 created at `timestamp`, asking for deletion reports
// at ipn:1.2.9 (behind the relay's peer), whose Bundle Age block its source
// encrypted under `enc_key`. One without a creation time must carry the
// block (RFC 9171 §4.4.2).
#[cfg(feature = "rfc9173")]
fn with_encrypted_bundle_age(enc_key: &Key, timestamp: CreationTimestamp) -> Parsed {
    let source: Eid = "ipn:1.3.1".parse().unwrap();
    let (_, data) = Builder::new(source.clone(), "ipn:1.2.99".parse().unwrap())
        .with_flags(BundleFlags {
            delete_report_requested: true,
            ..Default::default()
        })
        .with_report_to("ipn:1.2.9".parse().unwrap())
        .add_extension_block(block::Type::BundleAge)
        .expect("add the Bundle Age block")
        .build(emit(&5_000u64).0.into())
        .with_payload(b"relay me".as_slice().into())
        .build(timestamp)
        .expect("build the bundle");
    let built = hardy_bpv7::parse::parse(Bytes::from(data)).unwrap();
    let age = *built
        .bundle
        .blocks
        .iter()
        .find(|(_, b)| b.block_type == block::Type::BundleAge)
        .expect("the Bundle Age block is present")
        .0;
    let flags = ScopeFlags {
        include_security_header: false,
        ..ScopeFlags::default()
    };
    let mut encryptor = Encryptor::new(&built.bundle, &built.data);
    encryptor
        .encrypt_block(age, encryptor::Context::AES_GCM(flags), source, enc_key)
        .expect("encrypt the Bundle Age block");
    hardy_bpv7::parse::parse(Bytes::from(
        encryptor.rebuild().expect("rebuild the encrypted bundle"),
    ))
    .unwrap()
}

// A relay without the key to a clockless bundle's encrypted Bundle Age block
// cannot process the bundle, and deletes it for the reason RFC 9171 §5.6
// Step 4 gives a block the node cannot process, "Block unsupported", not the
// malformed block's "Block unintelligible" (Step 3).
#[cfg(feature = "rfc9173")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn keyless_relay_reports_an_unreadable_bundle_age_as_unsupported() {
    let received = with_encrypted_bundle_age(&enc_key(), CreationTimestamp::default());
    assert_eq!(
        relay_refusal(&received, Vec::new()).await,
        ReasonCode::BlockUnsupported
    );
}

// RFC 9171 §5.5: a relay gives a bundle with a creation time and no Bundle
// Age block one from the clock, for a downstream node without a clock, when
// a Classifier marks it; an unmarked bundle leaves without one.
#[cfg(feature = "rfc9173")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_classifier_has_a_relay_add_a_bundle_age() {
    let created = DtnTime::saturating_from(OffsetDateTime::now_utc() - TimeDuration::seconds(10));
    let (_, data) = Builder::new("ipn:1.3.1".parse().unwrap(), "ipn:1.2.99".parse().unwrap())
        .with_payload(b"relay me".as_slice().into())
        .build(CreationTimestamp::from_parts(Some(created), 1))
        .expect("build the bundle");
    let received = Bytes::from(data);

    let out = relay_with(&received, Vec::new(), |builder| {
        let mut pack = FilterPack::new("test");
        pack.ingress_classifier("bundle-age", AddsBundleAge);
        builder.add_filters(pack)
    })
    .await;
    let age = extension(&out, block::Type::BundleAge)
        .extract::<u64>(&out.data)
        .expect("the bundle age decodes")
        .expect("the bundle age is resident");
    assert!(age >= 10_000, "the age is the clock's: {age} ms");

    let out = relay(&received, Vec::new()).await;
    assert!(
        !out.bundle
            .blocks
            .values()
            .any(|b| b.block_type == block::Type::BundleAge),
        "an unmarked bundle leaves without a Bundle Age"
    );
}

// Marks every bundle for a Bundle Age at egress.
#[cfg(feature = "rfc9173")]
struct AddsBundleAge;

#[cfg(feature = "rfc9173")]
impl Classifier for AddsBundleAge {
    fn classify(&self, _: &ClassifyContext<'_>) -> Verdict<MetadataDelta> {
        let mut delta = MetadataDelta::default();
        delta.add_bundle_age = Some(true);
        Verdict::Continue(delta)
    }
}

// Declines every bundle a Bundle Age at egress.
#[cfg(feature = "rfc9173")]
struct DeclinesBundleAge;

#[cfg(feature = "rfc9173")]
impl Classifier for DeclinesBundleAge {
    fn classify(&self, _: &ClassifyContext<'_>) -> Verdict<MetadataDelta> {
        let mut delta = MetadataDelta::default();
        delta.add_bundle_age = Some(false);
        Verdict::Continue(delta)
    }
}

// A relay is the acceptor of every operation on a Bundle Age block, and on
// a bundle with a creation time one encrypted under a key it lacks is no
// failure: the block is discarded with its BCB operation, and the bundle is
// not held back for it. A node upstream wanted the bundle to carry a Bundle
// Age, so egress writes one from the clock in its place, in plaintext,
// unless a Classifier declines it.
#[cfg(feature = "rfc9173")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_keyless_relay_replaces_an_unreadable_bundle_age() {
    let created = DtnTime::saturating_from(OffsetDateTime::now_utc() - TimeDuration::seconds(10));
    let received =
        with_encrypted_bundle_age(&enc_key(), CreationTimestamp::from_parts(Some(created), 1));
    let unprotected = |out: &Parsed| {
        out.bundle
            .blocks
            .values()
            .all(|b| b.bcb.is_none() && b.block_type != block::Type::BlockSecurity)
    };

    let out = relay(&received.data, Vec::new()).await;
    assert!(unprotected(&out), "the BCB goes with the block it covered");
    let age = extension(&out, block::Type::BundleAge)
        .extract::<u64>(&out.data)
        .expect("the bundle age decodes")
        .expect("the bundle age is resident");
    assert!(
        age >= 10_000,
        "the age is the clock's, not the discarded block's: {age} ms"
    );

    let out = relay_with(&received.data, Vec::new(), |builder| {
        let mut pack = FilterPack::new("test");
        pack.ingress_classifier("bundle-age", DeclinesBundleAge);
        builder.add_filters(pack)
    })
    .await;
    assert!(unprotected(&out), "the BCB goes with the block it covered");
    assert!(
        !out.bundle
            .blocks
            .values()
            .any(|b| b.block_type == block::Type::BundleAge),
        "a Classifier declines the replacement"
    );
}
