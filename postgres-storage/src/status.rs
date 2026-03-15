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

/// Error returned when a `BundleStatus` value cannot be represented in the postgres schema.
#[derive(Debug, thiserror::Error)]
pub enum StatusConversionError {
    #[error("peer ID {0} exceeds i32::MAX and cannot be stored")]
    PeerId(u32),
    #[error("queue ID {0} exceeds i32::MAX and cannot be stored")]
    QueueId(u32),
    #[error("ADU timestamp {0}ms exceeds i64::MAX and cannot be stored")]
    Timestamp(u64),
    #[error("ADU sequence number {0} exceeds i64::MAX and cannot be stored")]
    Sequence(u64),
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
    fn with_kind(status: BundleStatusKind) -> Self {
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
                peer: u32::try_from(self.peer_id?).ok()?,
                queue: self.queue_id.and_then(|q| u32::try_from(q).ok()),
            }),
            BundleStatusKind::AduFragment => {
                let source: hardy_bpv7::eid::Eid = self.adu_source?.parse().ok()?;
                let creation_time = self
                    .adu_ts_ms
                    .filter(|&ms| ms != 0)
                    .and_then(|ms| u64::try_from(ms).ok())
                    .map(hardy_bpv7::dtn_time::DtnTime::new);
                let sequence_number = u64::try_from(self.adu_ts_seq?).ok()?;
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

/// Fallible because peer/queue IDs (u32) must fit in postgres `int4` (i32).
impl TryFrom<&BundleStatus> for StatusFields {
    type Error = StatusConversionError;

    fn try_from(status: &BundleStatus) -> Result<Self, Self::Error> {
        Ok(match status {
            BundleStatus::New => Self::with_kind(BundleStatusKind::New),
            BundleStatus::Waiting => Self::with_kind(BundleStatusKind::Waiting),
            BundleStatus::Dispatching => Self::with_kind(BundleStatusKind::Dispatching),
            BundleStatus::ForwardPending { peer, queue } => Self {
                peer_id: Some(
                    i32::try_from(*peer).map_err(|_| StatusConversionError::PeerId(*peer))?,
                ),
                queue_id: queue
                    .map(|q| i32::try_from(q).map_err(|_| StatusConversionError::QueueId(q)))
                    .transpose()?,
                ..Self::with_kind(BundleStatusKind::ForwardPending)
            },
            BundleStatus::AduFragment { source, timestamp } => {
                let ms = timestamp.creation_time().map_or(0, |t| t.millisecs());
                let seq = timestamp.sequence_number();
                Self {
                    adu_source: Some(source.to_string()),
                    adu_ts_ms: Some(
                        i64::try_from(ms).map_err(|_| StatusConversionError::Timestamp(ms))?,
                    ),
                    adu_ts_seq: Some(
                        i64::try_from(seq).map_err(|_| StatusConversionError::Sequence(seq))?,
                    ),
                    ..Self::with_kind(BundleStatusKind::AduFragment)
                }
            }
            BundleStatus::WaitingForService { service } => Self {
                service_eid: Some(service.to_string()),
                ..Self::with_kind(BundleStatusKind::WaitingForService)
            },
        })
    }
}
