use super::*;

struct Shared {
    cla: Arc<dyn Cla>,
    dispatcher: Arc<dispatcher::Dispatcher>,
    peer: u32,
    cla_addr: ClaAddress,
}

struct EgressQueue {
    shared: Arc<Shared>,
    lane: Option<u32>,
}

#[async_trait]
impl policy::EgressQueue for EgressQueue {
    async fn forward(&self, bundle: bundle::Bundle) {
        self.shared
            .dispatcher
            .forward_bundle(
                &*self.shared.cla,
                self.shared.peer,
                self.lane,
                &self.shared.cla_addr,
                bundle,
            )
            .await
    }
}

impl EgressQueue {
    fn create(shared: Arc<Shared>, lane: Option<u32>) -> Arc<dyn policy::EgressQueue> {
        Arc::new(Self { shared, lane })
    }
}

pub fn new_queue_set(
    cla: Arc<dyn Cla>,
    dispatcher: Arc<dispatcher::Dispatcher>,
    peer: u32,
    cla_addr: ClaAddress,
    lane_count: Option<core::num::NonZeroU32>,
) -> policy::EgressQueueSet {
    // A queue is instantiated eagerly per declared lane, so the count a CLA
    // declares directly sizes an allocation here — MAX_LANE_COUNT caps it to
    // keep an absurd declaration from becoming a resource bomb, and is public
    // so wire bridges reject with the same bound the BPA clamps to. Lane
    // indices are u32 on the trait surface; an over-declared count is clamped
    // rather than wrapped.
    let declared = lane_count.map_or(0, |n| n.get());
    let lane_count = declared.min(MAX_LANE_COUNT);
    if declared > lane_count {
        warn!("CLA declared {declared} egress lanes, clamped to {MAX_LANE_COUNT}");
    }
    let shared = Arc::new(Shared {
        cla,
        dispatcher,
        peer,
        cla_addr,
    });

    policy::EgressQueueSet {
        next_free: EgressQueue::create(shared.clone(), None),
        pinned: (0..lane_count)
            .map(|i| (i, EgressQueue::create(shared.clone(), Some(i))))
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use core::num::{NonZeroU32, NonZeroU64, NonZeroUsize};

    use hardy_bpv7::eid::{IpnNodeId, NodeId};

    use super::*;
    use crate::{
        filter::FilterEngine,
        keys::NullKeyProvider,
        node_ids::NodeIds,
        storage::{BundleMemStorage, MetadataMemStorage},
    };

    struct NullCla;

    #[async_trait]
    impl Cla for NullCla {
        async fn on_register(
            &self,
            _sink: Box<dyn Sink>,
            _node_ids: &[NodeId],
            _max_bundle_size: Option<NonZeroU64>,
        ) {
        }

        async fn on_unregister(&self) {}

        async fn forward(
            &self,
            _lane: Option<u32>,
            _cla_addr: &ClaAddress,
            _bundle_id: &Id,
            _total_len: u64,
            _stream: &mut dyn crate::stream::Receiver<Segment>,
        ) -> Result<ForwardBundleResult> {
            unreachable!("the clamp test never forwards")
        }
    }

    // An over-declared lane count is clamped to the public bound, so the
    // eager per-lane allocation cannot exceed it and the wire bridges'
    // reject bound stays the bound the BPA actually clamps to.
    #[tokio::test]
    async fn declared_lanes_are_clamped_to_the_public_bound() {
        let store = Arc::new(storage::store::Store::new(
            NonZeroUsize::new(16).unwrap(),
            Arc::new(MetadataMemStorage::new(None)),
            Arc::new(BundleMemStorage::new(None, None)),
        ));
        let node_ids = Arc::new(
            NodeIds::try_from(
                [NodeId::Ipn(IpnNodeId {
                    allocator_id: 0,
                    node_number: 1,
                })]
                .as_slice(),
            )
            .unwrap(),
        );
        let rib = routing::RibBuilder::new()
            .build(node_ids.clone(), store.clone())
            .await
            .unwrap();
        let (dispatcher, _wire) = dispatcher::Dispatcher::new(
            false,
            NonZeroUsize::new(16).unwrap(),
            NonZeroUsize::new(4).unwrap(),
            None,
            node_ids,
            store,
            rib,
            Arc::new(NullKeyProvider),
            Arc::new(FilterEngine::new()),
        );

        let set = new_queue_set(
            Arc::new(NullCla),
            dispatcher,
            7,
            ClaAddress::Private("peer".as_bytes().into()),
            NonZeroU32::new(MAX_LANE_COUNT + 1),
        );
        assert_eq!(set.pinned.len(), MAX_LANE_COUNT as usize);
        assert!(set.pinned.contains_key(&(MAX_LANE_COUNT - 1)));
        assert!(!set.pinned.contains_key(&MAX_LANE_COUNT));
    }
}
