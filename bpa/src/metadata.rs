use super::*;
use serde::{Deserialize, Serialize};
use serde_with::{
    base64::{Base64, UrlSafe},
    formats::Unpadded,
    serde_as,
};

#[serde_as]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BundleMetadata {
    pub storage_name: Option<Arc<str>>,

    #[serde_as(as = "Option<Base64<UrlSafe, Unpadded>>")]
    pub hash: Option<Arc<[u8]>>,

    pub received_at: Option<time::OffsetDateTime>,
}
