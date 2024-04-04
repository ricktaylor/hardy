use super::*;

pub struct Metadata {
    pub status: BundleStatus,
    pub storage_name: String,
    pub hash: String,
    pub received_at: time::OffsetDateTime,
}
