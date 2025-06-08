use super::*;

#[derive(Debug, Clone)]
pub struct Bundle {
    pub bundle: hardy_bpv7::bundle::Bundle,
    pub metadata: metadata::BundleMetadata,
}

impl Bundle {
    pub fn creation_time(&self) -> time::OffsetDateTime {
        self.bundle.id.timestamp.creation_time.map_or_else(
            || {
                self.metadata
                    .received_at
                    .unwrap_or_else(time::OffsetDateTime::now_utc)
                    // The following unwrap() is safe, as bundle.age is u64::MAX millisecs
                    .saturating_sub(self.bundle.age.unwrap_or_default().try_into().unwrap())
            },
            |creation_time| creation_time.into(),
        )
    }

    pub fn expiry(&self) -> time::OffsetDateTime {
        self.creation_time()
            // The following unwrap() is safe, as bundle.lifetime is u64::MAX millisecs
            .saturating_add(self.bundle.lifetime.try_into().unwrap())
    }

    pub fn has_expired(&self) -> bool {
        self.expiry() <= time::OffsetDateTime::now_utc()
    }
}
