use hardy_bpv7::prelude as bpv7;

#[derive(Debug)]
pub struct Metadata {
    pub status: BundleStatus,
    pub storage_name: String,
    pub hash: Vec<u8>,
    pub received_at: Option<time::OffsetDateTime>,
}

#[derive(Debug, Clone)]
pub enum BundleStatus {
    IngressPending,
    DispatchPending,
    ReassemblyPending,
    CollectionPending,
    ForwardPending,
    ForwardAckPending(u32, time::OffsetDateTime),
    Waiting(time::OffsetDateTime),
    Tombstone,
}

#[derive(Debug)]
pub struct Bundle {
    pub bundle: bpv7::Bundle,
    pub metadata: Metadata,
}

impl Bundle {
    fn millis_to_duration(ms: u64) -> time::Duration {
        time::Duration::saturating_seconds_f64(
            (ms / 1_000) as f64 + ((ms % 1_0000) as f64 / 1_000f64),
        )
    }

    pub fn creation_time(&self) -> time::OffsetDateTime {
        if let Some(creation_time) = self.bundle.id.timestamp.creation_time {
            creation_time.into()
        } else {
            self.metadata
                .received_at
                .unwrap_or_else(time::OffsetDateTime::now_utc)
                .saturating_sub(Self::millis_to_duration(self.bundle.age.unwrap_or(0)))
        }
    }

    pub fn expiry(&self) -> time::OffsetDateTime {
        self.creation_time()
            .saturating_add(Self::millis_to_duration(self.bundle.lifetime))
    }

    pub fn has_expired(&self) -> bool {
        self.expiry() <= time::OffsetDateTime::now_utc()
    }
}
