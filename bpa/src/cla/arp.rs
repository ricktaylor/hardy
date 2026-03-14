use super::*;
use hardy_bpv7::{
    bundle::Flags,
    creation_timestamp::CreationTimestamp,
    eid::Eid,
    hop_info::HopInfo,
    status_report::AdministrativeRecord,
};
use trace_err::*;

/// BP-ARP probe/retry interval and retry count.
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(default, rename_all = "kebab-case"))]
#[derive(Debug, Clone)]
pub struct ArpConfig {
    /// How long to wait between probe retries (in whole seconds).
    pub probe_interval_secs: u64,
    /// How many times to retry a probe before giving up.
    pub retry_count: u32,
    /// When to probe a CLA peer for its EID.
    pub policy: ArpPolicy,
}

impl Default for ArpConfig {
    fn default() -> Self {
        Self {
            probe_interval_secs: 30,
            retry_count: 5,
            policy: ArpPolicy::default(),
        }
    }
}

impl ArpConfig {
    pub fn probe_interval(&self) -> time::Duration {
        time::Duration::seconds(self.probe_interval_secs as i64)
    }
}

/// Controls when BP-ARP probing is triggered.
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "kebab-case"))]
#[derive(Debug, Clone, Default)]
pub enum ArpPolicy {
    /// Probe only when the peer's EID is unknown (Neighbour). This is the default.
    #[default]
    AsNeeded,
    /// Always probe every new CLA peer, even if the EID is already known.
    Always,
    /// Never send probes (BP-ARP disabled).
    Never,
}

struct NeighbourState {
    _task: hardy_async::JoinHandle<()>,
}

/// Manages BP-ARP probe tasks for Neighbours.
///
/// A Neighbour is a CLA peer whose EID is not yet known. The ARP subsystem sends
/// periodic `BpArpProbe` admin bundles directly over the CLA (bypassing the RIB)
/// and waits for a `BpArpAck` response that reveals the remote EID.
pub struct ArpSubsystem {
    config: ArpConfig,
    node_ids: Arc<node_ids::NodeIds>,
    neighbours: hardy_async::sync::spin::Mutex<HashMap<u32, NeighbourState>>,
}

impl ArpSubsystem {
    pub fn new(config: ArpConfig, node_ids: Arc<node_ids::NodeIds>) -> Arc<Self> {
        Arc::new(Self {
            config,
            node_ids,
            neighbours: hardy_async::sync::spin::Mutex::new(HashMap::new()),
        })
    }

    /// Called when a Neighbour (peer with unknown EID) is added to the CLA registry.
    /// Starts a probe-retry task unless `policy` is `Never`.
    pub async fn on_neighbour_added(
        self: &Arc<Self>,
        peer_id: u32,
        cla: Arc<registry::Cla>,
        cla_addr: ClaAddress,
        tasks: &hardy_async::TaskPool,
    ) {
        if matches!(self.config.policy, ArpPolicy::Never) {
            return;
        }

        let this = self.clone();
        let probe_bytes = self.build_probe();
        let retry_count = self.config.retry_count;
        let probe_interval = self.config.probe_interval();

        let task = tasks.spawn(async move {
            for attempt in 1..=retry_count {
                debug!("BP-ARP: sending probe to peer {peer_id} (attempt {attempt}/{retry_count})");
                if let Err(e) = cla.forward_raw(&cla_addr, probe_bytes.clone()).await {
                    warn!("BP-ARP: probe send failed for peer {peer_id}: {e}");
                }
                hardy_async::time::sleep(probe_interval).await;

                // Check if already resolved (abort() races with sleep completion)
                if !this.neighbours.lock().contains_key(&peer_id) {
                    return;
                }
            }
            warn!("BP-ARP: exhausted {retry_count} probe retries for peer {peer_id}, giving up");
            this.neighbours.lock().remove(&peer_id);
        });

        self.neighbours
            .lock()
            .insert(peer_id, NeighbourState { _task: task });
    }

    /// Called when a Neighbour is removed from the CLA registry (CLA disconnected).
    pub async fn on_neighbour_removed(&self, peer_id: u32) {
        // Dropping the JoinHandle does not abort the task, so we explicitly abort.
        if let Some(state) = self.neighbours.lock().remove(&peer_id) {
            state._task.abort();
        }
    }

    /// Called when a `BpArpAck` (or `BpArpProbe`) is received that resolves a Neighbour.
    /// Cancels the outstanding probe task.
    pub async fn on_ack_received(&self, peer_id: u32) {
        if let Some(state) = self.neighbours.lock().remove(&peer_id) {
            state._task.abort();
        }
    }

    /// Builds a `BpArpProbe` admin bundle as raw bytes, ready to send directly over a CLA.
    ///
    /// - Source: our admin endpoint (e.g. `ipn:42.0`)
    /// - Destination: `ipn:!.0` (LocalNode) so the remote BPA routes it to its admin endpoint
    /// - `is_admin_record` flag set
    /// - Hop Count block (limit 1) per spec §4.2 to prevent forwarding beyond the neighbour
    fn build_probe(&self) -> Bytes {
        let source = self.node_ids.get_admin_endpoint(&Eid::LocalNode(0));
        let destination = Eid::LocalNode(0);
        let payload = hardy_cbor::encode::emit(&AdministrativeRecord::BpArpProbe).0;

        let (_, data) = hardy_bpv7::builder::Builder::new(source, destination)
            .with_flags(Flags {
                is_admin_record: true,
                ..Default::default()
            })
            .with_hop_count(&HopInfo { limit: 1, count: 0 })
            .with_payload(payload.into())
            .build(CreationTimestamp::now())
            .trace_expect("Failed to build BpArpProbe bundle");

        Bytes::from(data)
    }

    /// Builds a `BpArpAck` admin bundle as raw bytes, addressed to the given destination.
    ///
    /// The payload contains a CBOR array of ALL our admin endpoint EIDs so the probing node
    /// can install routes for every scheme we support (§4.3).
    pub fn build_ack(&self, destination: &Eid) -> Bytes {
        let source = self.node_ids.get_admin_endpoint(destination);
        let eids = self.node_ids.get_all_admin_endpoints();
        let payload = hardy_cbor::encode::emit(&AdministrativeRecord::BpArpAck(eids)).0;

        let (_, data) = hardy_bpv7::builder::Builder::new(source, destination.clone())
            .with_flags(Flags {
                is_admin_record: true,
                ..Default::default()
            })
            .with_payload(payload.into())
            .build(CreationTimestamp::now())
            .trace_expect("Failed to build BpArpAck bundle");

        Bytes::from(data)
    }
}
