use super::*;

#[derive(Debug, Default, Clone)]
pub struct BundleMetadata {
    pub status: BundleStatus,
    pub storage_name: Option<Arc<str>>,
    pub hash: Option<Arc<[u8]>>,
    pub received_at: Option<time::OffsetDateTime>,
}

#[derive(Debug, Default, Clone, Eq, PartialEq)]
pub enum BundleStatus {
    #[default]
    DispatchPending,
    ReassemblyPending,
    Tombstone(time::OffsetDateTime),
}
