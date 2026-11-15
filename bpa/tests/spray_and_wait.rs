//! Integration test for Spray-and-Wait opportunistic routing.
//!
//! A bundle is injected with an experimental extension block specifying $L$
//! permits (initial value $L = 4$). As opportunistic peer contacts connect:
//! - The first two third-party stations connect and each receive a forwarded
//!   copy with their share of permits (e.g. 2 copies to station 1, 1 copy to
//!   station 2).
//! - Upon exhausting opportunistic forwarding permits (leaving $L = 1$, entering
//!   the Wait phase), a third third-party station connects and receives nothing.
//! - Finally, the target destination BPA connects and receives the bundle.

use core::num::NonZeroU64;
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Duration,
};

use flume::{Receiver as FlumeReceiver, Sender as FlumeSender, bounded};
use tokio::time::timeout;

use hardy_async::sync::spin::Once;
use hardy_bpa::{
    Bytes, async_trait,
    bpa::{Bpa, BpaRegistration},
    cla::{
        Acceptance, Cla, ClaAddress, ClaInit, ForwardBundleResult, Result as ClaResult, Sink,
        TransferOutcome,
    },
    filter::{
        Boundary, Classifier, ClassifyContext, RewriteContext, Rewriter, Verdict, pack::FilterPack,
        slots::MetadataDelta,
    },
    routing::{RouteAction, RoutingAgent, RoutingSink},
    stream::{Receiver, Segment, buffer_stream},
};
use hardy_bpv7::{
    block::Type as BlockType,
    builder::Builder,
    bundle::Id as BundleId,
    creation_timestamp::CreationTimestamp,
    eid::{Eid, IpnNodeId, NodeId},
    parse::parse,
    reader::{PlainReader, ReaderExt},
};
use hardy_cbor::encode::emit;
use hardy_eid_patterns::EidPattern;

/// Experimental extension block type used for Spray-and-Wait permit count $L$.
/// RFC 9171 §4.3.1 reserves block types 192–255 for experimental and private use.
/// Code 194 corresponds to the Spray-and-Wait permits block type in ION-DTN.
const SPRAY_AND_WAIT_BLOCK_TYPE: u64 = 194;

hardy_bpa::slot!(static SPRAY_L: u32);

/// Ingress classifier that identifies bundles containing the Spray-and-Wait
/// extension block and records the received $L$ value into the `SPRAY_L` slot
/// and tracks it in `updated_slots`.
struct SnwIngressClassifier {
    updated_slots: Arc<Mutex<HashMap<BundleId, u32>>>,
}

impl Classifier for SnwIngressClassifier {
    fn classify(&self, ctx: &ClassifyContext<'_>) -> Verdict<MetadataDelta> {
        let snw_block = ctx
            .bundle()
            .blocks
            .iter()
            .find(|(_, b)| b.block_type == BlockType::Unrecognised(SPRAY_AND_WAIT_BLOCK_TYPE));

        if let Some((&block_num, _)) = snw_block
            && let Ok(Some(l)) = ctx.reader().extract::<u32>(block_num)
        {
            self.updated_slots
                .lock()
                .unwrap()
                .insert(ctx.bundle().primary.id.clone(), l);
            let mut delta = MetadataDelta::default();
            delta.set(&SPRAY_L, &l);
            return Verdict::Continue(delta);
        }

        Verdict::Continue(MetadataDelta::default())
    }
}

/// Egress rewriter that updates the Spray-and-Wait extension block with
/// `floor(L / 2)` before transmission and updates the tracked $L$ value.
///
/// If $L == 1$ and the destination is not the peer (next hop), the bundle is
/// not rewritten for relaying.
struct SnwEgressRewriter {
    updated_slots: Arc<Mutex<HashMap<BundleId, u32>>>,
}

impl Rewriter for SnwEgressRewriter {
    fn rewrite(&self, ctx: &mut RewriteContext<'_>) {
        let Boundary::Egress { next_hop } = ctx.boundary() else {
            return;
        };

        let snw_block = ctx
            .bundle()
            .blocks
            .iter()
            .find(|(_, b)| b.block_type == BlockType::Unrecognised(SPRAY_AND_WAIT_BLOCK_TYPE));
        let Some((&block_num, _)) = snw_block else {
            return;
        };

        let bundle_id = &ctx.bundle().primary.id;
        let current_l = self
            .updated_slots
            .lock()
            .unwrap()
            .get(bundle_id)
            .copied()
            .or_else(|| ctx.metadata().slot(&SPRAY_L))
            .or_else(|| ctx.reader().extract::<u32>(block_num).ok().flatten())
            .unwrap_or(1);

        // Direct delivery to final target does not consume spray permits
        if ctx.bundle().primary.destination == *next_hop {
            return;
        }

        // Rejection rule: if L <= 1 and destination is not the peer, do not relay
        if current_l <= 1 {
            return;
        }

        // Update the extension block with floor(L / 2) for the forwarded copy
        let sent_l = current_l / 2;
        let cbor_sent_l = emit(&sent_l).0;
        let _ = ctx.editor().replace(block_num, cbor_sent_l.into());

        // Update the remaining L value in the local slot tracker: current_l - sent_l
        let remaining_l = current_l - sent_l;
        self.updated_slots
            .lock()
            .unwrap()
            .insert(bundle_id.clone(), remaining_l);
    }
}

/// Simulated CLA capturing forwarded bundles into an asynchronous flume channel.
///
/// Returns [`ForwardBundleResult::Sent`] unconditionally without inspecting
/// bundle destination or DTN routing headers.
struct MockCla {
    sink: Once<Box<dyn Sink>>,
    forwarded_tx: FlumeSender<Bytes>,
    async_accept: bool,
}

impl MockCla {
    fn new() -> (Arc<Self>, FlumeReceiver<Bytes>) {
        Self::with_async_accept(false)
    }

    fn new_async() -> (Arc<Self>, FlumeReceiver<Bytes>) {
        Self::with_async_accept(true)
    }

    fn with_async_accept(async_accept: bool) -> (Arc<Self>, FlumeReceiver<Bytes>) {
        let (tx, rx) = bounded(16);
        (
            Arc::new(Self {
                sink: Once::new(),
                forwarded_tx: tx,
                async_accept,
            }),
            rx,
        )
    }
}

#[async_trait]
impl Cla for MockCla {
    async fn on_register(
        &self,
        sink: Box<dyn Sink>,
        _node_ids: &[NodeId],
        _max_bundle_size: Option<NonZeroU64>,
    ) {
        self.sink.call_once(|| sink);
    }

    async fn on_unregister(&self) {}

    async fn forward(
        &self,
        _lane: Option<u32>,
        _cla_addr: &ClaAddress,
        _bundle_id: &BundleId,
        total_len: u64,
        stream: &mut dyn Receiver<Segment>,
    ) -> ClaResult<ForwardBundleResult> {
        let bundle = buffer_stream(stream, total_len).await?;
        let _ = self.forwarded_tx.send(bundle);
        if self.async_accept {
            Ok(ForwardBundleResult::Accepted)
        } else {
            Ok(ForwardBundleResult::Sent)
        }
    }
}

/// Routing Agent for Spray-and-Wait opportunistic routing.
///
/// Pushes opportunistic `ForwardReplicate(...)` routes during the Spray phase ($L > 1$),
/// instructing the BPA to retain the bundle in storage after forwarding.
/// Refuses to install opportunistic routes once the Wait phase is reached ($L = 1$).
struct SnwRoutingAgent {
    sink: Once<Box<dyn RoutingSink>>,
    updated_slots: Arc<Mutex<HashMap<BundleId, u32>>>,
}

impl SnwRoutingAgent {
    fn new(updated_slots: Arc<Mutex<HashMap<BundleId, u32>>>) -> Self {
        Self {
            sink: Once::new(),
            updated_slots,
        }
    }

    /// Try to add an opportunistic spray route toward `via_eid` for `target_pattern`.
    ///
    /// If $L \le 1$, the Wait phase has been reached and no opportunistic relay
    /// route will be installed.
    async fn add_spray_route(
        &self,
        target_pattern: EidPattern,
        via_eid: Eid,
        bundle_id: &BundleId,
    ) -> bool {
        let current_l = self
            .updated_slots
            .lock()
            .unwrap()
            .get(bundle_id)
            .copied()
            .unwrap_or(1);

        if current_l <= 1 {
            return false;
        }

        if let Some(sink) = self.sink.get() {
            let _ = sink
                .add_route(target_pattern, RouteAction::ForwardReplicate(via_eid), 100)
                .await;
            return true;
        }
        false
    }

    /// Remove a previously installed spray route.
    async fn remove_spray_route(&self, target_pattern: &EidPattern, via_eid: &Eid) {
        if let Some(sink) = self.sink.get() {
            let _ = sink
                .remove_route(
                    target_pattern,
                    &RouteAction::ForwardReplicate(via_eid.clone()),
                    100,
                )
                .await;
        }
    }
}

#[async_trait]
impl RoutingAgent for SnwRoutingAgent {
    async fn on_register(&self, sink: Box<dyn RoutingSink>, _node_ids: &[NodeId]) {
        self.sink.call_once(|| sink);
    }

    async fn on_unregister(&self) {}
}

/// Helper building a bundle with the experimental Spray-and-Wait block ($L$ permits).
fn build_spray_and_wait_bundle(
    source: &Eid,
    destination: &Eid,
    initial_l: u32,
    payload: &[u8],
) -> (BundleId, Bytes) {
    let cbor_l = emit(&initial_l).0;

    let (bundle, data) = Builder::new(source.clone(), destination.clone())
        .add_extension_block(BlockType::Unrecognised(SPRAY_AND_WAIT_BLOCK_TYPE))
        .expect("adding experimental extension block must succeed")
        .build(cbor_l.into())
        .add_extension_block(BlockType::Payload)
        .expect("adding payload block must succeed")
        .build(payload.into())
        .build(CreationTimestamp::now())
        .expect("bundle build must succeed");

    (bundle.primary.id, Bytes::from(data))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn spray_and_wait_routing() {
    let updated_slots = Arc::new(Mutex::new(HashMap::new()));
    let snw_agent = Arc::new(SnwRoutingAgent::new(updated_slots.clone()));

    let mut pack = FilterPack::new("spray_and_wait");
    pack.ingress_classifier(
        "snw_ingress",
        SnwIngressClassifier {
            updated_slots: updated_slots.clone(),
        },
    );
    pack.egress_rewriter(
        "snw_egress",
        SnwEgressRewriter {
            updated_slots: updated_slots.clone(),
        },
    );

    let bpa = Bpa::builder()
        .add_filters(pack)
        .routing_agent("spray_and_wait", snw_agent.clone())
        .build()
        .await
        .expect("BPA build must succeed");
    bpa.start(false).await;

    // 1. Ingress CLA: simulate sending a bundle with initial L = 4 to target node 99.
    let (ingress_cla, _) = MockCla::new();
    bpa.register_cla(
        "cla_ingress".to_string(),
        ingress_cla.clone(),
        None,
        ClaInit::default(),
    )
    .await
    .expect("ingress CLA registration must succeed");

    let source_node = NodeId::Ipn(IpnNodeId {
        allocator_id: 0,
        node_number: 1,
    });
    let source_eid: Eid = "ipn:0.1.1".parse().unwrap();
    let target_node = NodeId::Ipn(IpnNodeId {
        allocator_id: 0,
        node_number: 99,
    });
    let target_dest_eid: Eid = "ipn:0.99.1".parse().unwrap();

    let initial_l = 4u32;
    let (bundle_id, mut bundle_stream) = build_spray_and_wait_bundle(
        &source_eid,
        &target_dest_eid,
        initial_l,
        b"Spray-and-Wait test payload",
    );

    let acceptance = ingress_cla
        .sink
        .get()
        .expect("ingress CLA sink available")
        .dispatch(Some(&source_node), None, &mut bundle_stream)
        .await
        .expect("ingress dispatch must succeed");
    assert_eq!(acceptance, Acceptance::Accepted);

    // 2. First third-party station connects (node 2) via simulated CLA.
    let (cla_station_1, station_1_rx) = MockCla::new();
    bpa.register_cla(
        "cla_station_1".to_string(),
        cla_station_1.clone(),
        None,
        ClaInit::default(),
    )
    .await
    .expect("station 1 CLA registration must succeed");

    let station_1_node = NodeId::Ipn(IpnNodeId {
        allocator_id: 0,
        node_number: 2,
    });
    let station_1_eid: Eid = "ipn:0.2.0".parse().unwrap();
    let peer_addr_1 = ClaAddress::Private(Bytes::from_static(b"station_1"));
    cla_station_1
        .sink
        .get()
        .expect("station 1 sink available")
        .add_peer(peer_addr_1, &[station_1_node])
        .await
        .expect("add_peer for station 1 must succeed");

    let target_pattern: EidPattern = "ipn:0.99.*".parse().unwrap();
    let added_1 = snw_agent
        .add_spray_route(target_pattern.clone(), station_1_eid.clone(), &bundle_id)
        .await;
    assert!(added_1, "must add spray route for station 1");

    // Station 1 must receive a copy of the bundle with L = 2.
    // Event-driven wait; the timeout only bounds a regression.
    let bundle_copy_1 = timeout(Duration::from_secs(5), station_1_rx.recv_async())
        .await
        .expect("timeout waiting for station 1 copy")
        .expect("station 1 channel closed");

    let parsed_1 = parse(bundle_copy_1).expect("station 1 bundle must be valid BPv7");
    let (&snw_block_num_1, _) = parsed_1
        .bundle
        .blocks
        .iter()
        .find(|(_, b)| b.block_type == BlockType::Unrecognised(SPRAY_AND_WAIT_BLOCK_TYPE))
        .expect("station 1 copy must contain the Spray-and-Wait extension block");
    let reader_1 = PlainReader {
        blocks: &parsed_1.bundle.blocks,
        source_data: &parsed_1.data,
    };
    let l_copy_1: u32 = reader_1
        .extract(snw_block_num_1)
        .expect("must read extension block")
        .expect("L value must be present");
    assert_eq!(
        l_copy_1, 2,
        "Station 1 must receive floor(4 / 2) = 2 permits"
    );

    snw_agent
        .remove_spray_route(&target_pattern, &station_1_eid)
        .await;

    // 3. Second third-party station connects (node 3) via simulated CLA.
    let (cla_station_2, station_2_rx) = MockCla::new();
    bpa.register_cla(
        "cla_station_2".to_string(),
        cla_station_2.clone(),
        None,
        ClaInit::default(),
    )
    .await
    .expect("station 2 CLA registration must succeed");

    let station_2_node = NodeId::Ipn(IpnNodeId {
        allocator_id: 0,
        node_number: 3,
    });
    let station_2_eid: Eid = "ipn:0.3.0".parse().unwrap();
    let peer_addr_2 = ClaAddress::Private(Bytes::from_static(b"station_2"));
    cla_station_2
        .sink
        .get()
        .expect("station 2 sink available")
        .add_peer(peer_addr_2, &[station_2_node])
        .await
        .expect("add_peer for station 2 must succeed");

    let added_2 = snw_agent
        .add_spray_route(target_pattern.clone(), station_2_eid.clone(), &bundle_id)
        .await;
    assert!(added_2, "must add spray route for station 2");

    // Station 2 must receive a copy of the bundle with L = 1.
    // Event-driven wait; the timeout only bounds a regression.
    let bundle_copy_2 = timeout(Duration::from_secs(5), station_2_rx.recv_async())
        .await
        .expect("timeout waiting for station 2 copy")
        .expect("station 2 channel closed");

    let parsed_2 = parse(bundle_copy_2).expect("station 2 bundle must be valid BPv7");
    let (&snw_block_num_2, _) = parsed_2
        .bundle
        .blocks
        .iter()
        .find(|(_, b)| b.block_type == BlockType::Unrecognised(SPRAY_AND_WAIT_BLOCK_TYPE))
        .expect("station 2 copy must contain the Spray-and-Wait extension block");
    let reader_2 = PlainReader {
        blocks: &parsed_2.bundle.blocks,
        source_data: &parsed_2.data,
    };
    let l_copy_2: u32 = reader_2
        .extract(snw_block_num_2)
        .expect("must read extension block")
        .expect("L value must be present");
    assert_eq!(
        l_copy_2, 1,
        "Station 2 must receive floor(2 / 2) = 1 permit"
    );

    snw_agent
        .remove_spray_route(&target_pattern, &station_2_eid)
        .await;

    // 4. A third BPA connects via a new CLA (node 4).
    // Opportunistic forwarding permits are now exhausted (Wait phase entered, L = 1).
    // It must receive nothing.
    let (cla_station_3, station_3_rx) = MockCla::new();
    bpa.register_cla(
        "cla_station_3".to_string(),
        cla_station_3.clone(),
        None,
        ClaInit::default(),
    )
    .await
    .expect("station 3 CLA registration must succeed");

    let station_3_node = NodeId::Ipn(IpnNodeId {
        allocator_id: 0,
        node_number: 4,
    });
    let station_3_eid: Eid = "ipn:0.4.0".parse().unwrap();
    let peer_addr_3 = ClaAddress::Private(Bytes::from_static(b"station_3"));
    cla_station_3
        .sink
        .get()
        .expect("station 3 sink available")
        .add_peer(peer_addr_3, &[station_3_node])
        .await
        .expect("add_peer for station 3 must succeed");

    let added_3 = snw_agent
        .add_spray_route(target_pattern, station_3_eid, &bundle_id)
        .await;
    assert!(
        !added_3,
        "Station 3 route must not be added once Wait phase (L = 1) is reached"
    );

    // 5. Finally, the target destination BPA (node 99) connects via a simulated CLA.
    let (cla_target, target_rx) = MockCla::new();
    bpa.register_cla(
        "cla_target".to_string(),
        cla_target.clone(),
        None,
        ClaInit::default(),
    )
    .await
    .expect("target CLA registration must succeed");

    let target_peer_addr = ClaAddress::Private(Bytes::from_static(b"target"));
    cla_target
        .sink
        .get()
        .expect("target CLA sink available")
        .add_peer(target_peer_addr, &[target_node])
        .await
        .expect("add_peer for target node must succeed");

    // The target BPA must receive its bundle (direct delivery during Wait phase).
    // Event-driven wait; the timeout only bounds a regression.
    let target_bundle = timeout(Duration::from_secs(5), target_rx.recv_async())
        .await
        .expect("timeout waiting for target bundle")
        .expect("target channel closed");

    let parsed_target = parse(target_bundle).expect("target bundle must be valid BPv7");
    assert_eq!(parsed_target.bundle.primary.destination, target_dest_eid);

    // 6. Verify absence on station 3:
    // Completed shutdown is the barrier proving nothing further was forwarded.
    bpa.shutdown().await;
    assert!(
        station_3_rx.is_empty(),
        "3rd BPA must not receive any bundle once the Wait phase is reached"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn forward_replicate_async_cla_retains_bundle() {
    let updated_slots = Arc::new(Mutex::new(HashMap::new()));
    let snw_agent = Arc::new(SnwRoutingAgent::new(updated_slots));

    let bpa = Bpa::builder()
        .routing_agent("spray_and_wait", snw_agent.clone())
        .build()
        .await
        .expect("BPA build must succeed");
    bpa.start(false).await;

    let (ingress_cla, _) = MockCla::new();
    bpa.register_cla(
        "cla_ingress".to_string(),
        ingress_cla.clone(),
        None,
        ClaInit::default(),
    )
    .await
    .expect("ingress CLA registration must succeed");

    let source_node = NodeId::Ipn(IpnNodeId {
        allocator_id: 0,
        node_number: 1,
    });
    let source_eid: Eid = "ipn:0.1.1".parse().unwrap();
    let target_node = NodeId::Ipn(IpnNodeId {
        allocator_id: 0,
        node_number: 99,
    });
    let target_dest_eid: Eid = "ipn:0.99.1".parse().unwrap();

    let (bundle_id, mut bundle_stream) =
        build_spray_and_wait_bundle(&source_eid, &target_dest_eid, 4, b"Async CLA retain test");

    let acceptance = ingress_cla
        .sink
        .get()
        .expect("ingress CLA sink available")
        .dispatch(Some(&source_node), None, &mut bundle_stream)
        .await
        .expect("ingress dispatch must succeed");
    assert_eq!(acceptance, Acceptance::Accepted);

    // Async CLA returns ForwardBundleResult::Accepted and later reports TransferOutcome::Completed.
    let (cla_station_1, station_1_rx) = MockCla::new_async();
    bpa.register_cla(
        "cla_station_1".to_string(),
        cla_station_1.clone(),
        None,
        ClaInit::default(),
    )
    .await
    .expect("station 1 CLA registration must succeed");

    let station_1_node = NodeId::Ipn(IpnNodeId {
        allocator_id: 0,
        node_number: 2,
    });
    let station_1_eid: Eid = "ipn:0.2.0".parse().unwrap();
    cla_station_1
        .sink
        .get()
        .expect("station 1 sink available")
        .add_peer(
            ClaAddress::Private(Bytes::from_static(b"station_1")),
            &[station_1_node],
        )
        .await
        .expect("add_peer for station 1 must succeed");

    let target_pattern: EidPattern = "ipn:0.99.*".parse().unwrap();
    let sink = snw_agent.sink.get().expect("routing sink available");
    sink.add_route(
        target_pattern.clone(),
        RouteAction::ForwardReplicate(station_1_eid.clone()),
        100,
    )
    .await
    .expect("add_route must succeed");

    // Event-driven wait; the timeout only bounds a regression.
    let forwarded = timeout(Duration::from_secs(5), station_1_rx.recv_async())
        .await
        .expect("timeout waiting for async station 1 copy")
        .expect("station 1 channel closed");
    let parsed_copy = parse(forwarded).expect("station 1 bundle must be valid BPv7");
    assert_eq!(parsed_copy.bundle.primary.id, bundle_id);

    // Remove the ForwardReplicate route *while* the transfer is still
    // ForwardAckPending (in-flight), then confirm deferred completion:
    // the original bundle must still remain in storage (Waiting), not be deleted.
    snw_agent
        .remove_spray_route(&target_pattern, &station_1_eid)
        .await;

    cla_station_1
        .sink
        .get()
        .expect("station 1 sink available")
        .transfer_outcome(&bundle_id, TransferOutcome::Completed)
        .await
        .expect("transfer_outcome must succeed");

    // Now the final target connects and must still receive the original bundle.
    let (cla_target, target_rx) = MockCla::new();
    bpa.register_cla(
        "cla_target".to_string(),
        cla_target.clone(),
        None,
        ClaInit::default(),
    )
    .await
    .expect("target CLA registration must succeed");
    cla_target
        .sink
        .get()
        .expect("target CLA sink available")
        .add_peer(
            ClaAddress::Private(Bytes::from_static(b"target")),
            &[target_node],
        )
        .await
        .expect("add_peer for target node must succeed");

    let target_bundle = timeout(Duration::from_secs(5), target_rx.recv_async())
        .await
        .expect("timeout waiting for target bundle")
        .expect("target channel closed");
    let parsed_target = parse(target_bundle).expect("target bundle must be valid BPv7");
    assert_eq!(parsed_target.bundle.primary.id, bundle_id);

    bpa.shutdown().await;
}
