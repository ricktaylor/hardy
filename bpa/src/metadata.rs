use super::*;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BundleMetadata {
    pub storage_name: Option<Arc<str>>,

    pub received_at: Option<time::OffsetDateTime>,
}
