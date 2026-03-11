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

/// Flat projection of the status-related columns, shared across all row types.
/// Derives `FromRow` so it can be embedded via `#[sqlx(flatten)]` in row structs,
/// and `TryFrom<&BundleStatus>` so it doubles as the SQL bind source (write path).
#[derive(sqlx::FromRow)]
pub struct StatusFields {
    pub status: BundleStatusKind,
    pub peer_id: Option<i32>,
    pub queue_id: Option<i32>,
    pub adu_source: Option<String>,
    /// Milliseconds since DTN epoch, or 0 when no DTN creation clock.
    pub adu_ts_ms: Option<i64>,
    pub adu_ts_seq: Option<i64>,
    pub service_eid: Option<String>,
}

impl StatusFields {
    fn for_kind(status: BundleStatusKind) -> Self {
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

    pub fn into_bundle_status(self) -> Option<BundleStatus> {
        match self.status {
            BundleStatusKind::New => Some(BundleStatus::New),
            BundleStatusKind::Waiting => Some(BundleStatus::Waiting),
            BundleStatusKind::Dispatching => Some(BundleStatus::Dispatching),
            BundleStatusKind::ForwardPending => Some(BundleStatus::ForwardPending {
                peer: self.peer_id? as u32,
                queue: self.queue_id.map(|q| q as u32),
            }),
            BundleStatusKind::AduFragment => {
                let source: hardy_bpv7::eid::Eid = self.adu_source?.parse().ok()?;
                let creation_time = self
                    .adu_ts_ms
                    .filter(|&ms| ms != 0)
                    .map(|ms| hardy_bpv7::dtn_time::DtnTime::new(ms as u64));
                let sequence_number = self.adu_ts_seq? as u64;
                let timestamp = hardy_bpv7::creation_timestamp::CreationTimestamp::from_parts(
                    creation_time,
                    sequence_number,
                );
                Some(BundleStatus::AduFragment { source, timestamp })
            }
            BundleStatusKind::WaitingForService => Some(BundleStatus::WaitingForService {
                service: self.service_eid?.parse().ok()?,
            }),
        }
    }
}

/// Fallible because `ForwardPending.peer` (u32) must fit in a postgres `int4` (i32).
impl TryFrom<&BundleStatus> for StatusFields {
    type Error = std::num::TryFromIntError;

    fn try_from(status: &BundleStatus) -> Result<Self, Self::Error> {
        Ok(match status {
            BundleStatus::New => Self::for_kind(BundleStatusKind::New),
            BundleStatus::Waiting => Self::for_kind(BundleStatusKind::Waiting),
            BundleStatus::Dispatching => Self::for_kind(BundleStatusKind::Dispatching),
            BundleStatus::ForwardPending { peer, queue } => Self {
                peer_id: Some(i32::try_from(*peer)?),
                queue_id: queue.map(i32::try_from).transpose()?,
                ..Self::for_kind(BundleStatusKind::ForwardPending)
            },
            BundleStatus::AduFragment { source, timestamp } => Self {
                adu_source: Some(source.to_string()),
                adu_ts_ms: Some(
                    timestamp
                        .creation_time()
                        .map_or(0, |t| t.millisecs() as i64),
                ),
                adu_ts_seq: Some(timestamp.sequence_number() as i64),
                ..Self::for_kind(BundleStatusKind::AduFragment)
            },
            BundleStatus::WaitingForService { service } => Self {
                service_eid: Some(service.to_string()),
                ..Self::for_kind(BundleStatusKind::WaitingForService)
            },
        })
    }
}
