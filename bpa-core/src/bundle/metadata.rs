use super::*;

pub struct Metadata {
    pub status: BundleStatus,
    pub storage_name: String,
    pub hash: Vec<u8>,
    pub received_at: Option<time::OffsetDateTime>,
}
