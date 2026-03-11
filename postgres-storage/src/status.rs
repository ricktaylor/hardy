use hardy_bpa::metadata::BundleStatus;

/// Mirrors the `bundle_status` postgres enum for type-safe binding and decoding.
/// `#[derive(sqlx::Type)]` generates `Encode`/`Decode` so sqlx maps the postgres
/// enum directly — no `::bundle_status` casts or `&'static str` conversions needed.
#[derive(Debug, Clone, Copy, sqlx::Type)]
#[sqlx(type_name = "bundle_status", rename_all = "snake_case")]
pub enum BundleStatusKind {
    New,
    Waiting,
    Dispatching,
    ForwardPending,
    AduFragment,
    WaitingForService,
}

/// Flat representation of a BundleStatus for binding to SQL parameters.
pub struct StatusParams {
    pub status: BundleStatusKind,
    pub peer_id: Option<i32>,
    pub queue_id: Option<i32>,
    pub adu_source: Option<String>,
    /// Milliseconds since DTN epoch, or 0 when no DTN creation clock.
    pub adu_ts_ms: Option<i64>,
    pub adu_ts_seq: Option<i64>,
    pub service_eid: Option<String>,
}

impl StatusParams {
    fn new(status: BundleStatusKind) -> Self {
        Self {
            status,
            peer_id: None,
            queue_id: None,
            adu_source: None,
            adu_ts_ms: None,
            adu_ts_seq: None,
            service_eid: None,
        }
    }
}

pub fn from_status(status: &BundleStatus) -> StatusParams {
    match status {
        BundleStatus::New => StatusParams::new(BundleStatusKind::New),
        BundleStatus::Waiting => StatusParams::new(BundleStatusKind::Waiting),
        BundleStatus::Dispatching => StatusParams::new(BundleStatusKind::Dispatching),
        BundleStatus::ForwardPending { peer, queue } => StatusParams {
            status: BundleStatusKind::ForwardPending,
            peer_id: Some(*peer as i32),
            queue_id: queue.map(|q| q as i32),
            ..StatusParams::new(BundleStatusKind::ForwardPending)
        },
        BundleStatus::AduFragment { source, timestamp } => StatusParams {
            status: BundleStatusKind::AduFragment,
            adu_source: Some(source.to_string()),
            // 0 encodes "no DTN creation clock" (same convention as sqlite-storage)
            adu_ts_ms: Some(
                timestamp
                    .creation_time()
                    .map_or(0, |t| t.millisecs() as i64),
            ),
            adu_ts_seq: Some(timestamp.sequence_number() as i64),
            ..StatusParams::new(BundleStatusKind::AduFragment)
        },
        BundleStatus::WaitingForService { service } => StatusParams {
            status: BundleStatusKind::WaitingForService,
            service_eid: Some(service.to_string()),
            ..StatusParams::new(BundleStatusKind::WaitingForService)
        },
    }
}

pub fn to_status(
    status: BundleStatusKind,
    peer_id: Option<i32>,
    queue_id: Option<i32>,
    adu_source: Option<String>,
    adu_ts_ms: Option<i64>,
    adu_ts_seq: Option<i64>,
    service_eid: Option<String>,
) -> Option<BundleStatus> {
    match status {
        BundleStatusKind::New => Some(BundleStatus::New),
        BundleStatusKind::Waiting => Some(BundleStatus::Waiting),
        BundleStatusKind::Dispatching => Some(BundleStatus::Dispatching),
        BundleStatusKind::ForwardPending => Some(BundleStatus::ForwardPending {
            peer: peer_id? as u32,
            queue: queue_id.map(|q| q as u32),
        }),
        BundleStatusKind::AduFragment => {
            let source: hardy_bpv7::eid::Eid = adu_source?.parse().ok()?;
            let creation_time = adu_ts_ms
                .filter(|&ms| ms != 0)
                .map(|ms| hardy_bpv7::dtn_time::DtnTime::new(ms as u64));
            let sequence_number = adu_ts_seq? as u64;
            let timestamp = hardy_bpv7::creation_timestamp::CreationTimestamp::from_parts(
                creation_time,
                sequence_number,
            );
            Some(BundleStatus::AduFragment { source, timestamp })
        }
        BundleStatusKind::WaitingForService => Some(BundleStatus::WaitingForService {
            service: service_eid?.parse().ok()?,
        }),
    }
}
