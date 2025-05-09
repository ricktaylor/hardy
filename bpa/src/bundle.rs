use super::*;

#[derive(Debug, Clone)]
pub struct Bundle {
    pub bundle: bpv7::Bundle,
    pub metadata: metadata::BundleMetadata,
}

impl Bundle {
    pub fn creation_time(&self) -> time::OffsetDateTime {
        self.bundle.id.timestamp.creation_time.map_or_else(
            || {
                self.metadata
                    .received_at
                    .unwrap_or_else(time::OffsetDateTime::now_utc)
                    .saturating_sub(self.bundle.age.unwrap_or_default())
            },
            |creation_time| creation_time.into(),
        )
    }

    pub fn expiry(&self) -> time::OffsetDateTime {
        self.creation_time().saturating_add(self.bundle.lifetime)
    }

    pub fn has_expired(&self) -> bool {
        self.expiry() <= time::OffsetDateTime::now_utc()
    }
}
