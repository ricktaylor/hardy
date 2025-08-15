use super::*;

#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "bincode", derive(bincode::Encode, bincode::Decode))]
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
                    // The following unwrap() is safe, as bundle.age is u64::MAX millisecs
                    .saturating_sub(self.bundle.age.unwrap_or_default().try_into().unwrap())
            },
            |creation_time| creation_time.into(),
        )
    }

    pub fn expiry(&self) -> time::OffsetDateTime {
        self.creation_time().saturating_add(
            self.bundle
                .lifetime
                .try_into()
                .unwrap_or(time::Duration::MAX),
        )
    }

    pub fn has_expired(&self) -> bool {
        self.expiry() <= time::OffsetDateTime::now_utc()
    }
}
