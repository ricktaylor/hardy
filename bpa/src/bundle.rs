use super::*;

#[derive(Debug, Clone)]
pub struct Bundle {
    pub bundle: bpv7::Bundle,
    pub metadata: metadata::BundleMetadata,
}

impl Bundle {
    pub fn creation_time(&self) -> time::OffsetDateTime {
        if let Some(creation_time) = self.bundle.id.timestamp.creation_time {
            creation_time.into()
        } else {
            self.metadata
                .received_at
                .unwrap_or_else(time::OffsetDateTime::now_utc)
                .saturating_sub(self.bundle.age.unwrap_or_default())
        }
    }

    pub fn expiry(&self) -> time::OffsetDateTime {
        self.creation_time().saturating_add(self.bundle.lifetime)
    }

    pub fn has_expired(&self) -> bool {
        self.expiry() <= time::OffsetDateTime::now_utc()
    }
}
