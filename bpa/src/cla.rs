use super::*;
use thiserror::Error;

pub type Result<T> = core::result::Result<T, Error>;

#[derive(Debug, Error)]
pub enum Error {
    #[error("A CLA with ident {0} already exists")]
    DuplicateClaIdent(String),

    #[error(transparent)]
    InvalidBundle(#[from] bpv7::Error),

    #[error("The CLA is shutting down")]
    Disconnected,

    #[error(transparent)]
    Internal(#[from] Box<dyn std::error::Error + Send + Sync>),
}

pub enum ForwardBundleResult {
    Sent,
    Pending(u32, Option<time::OffsetDateTime>),
    Congested(time::OffsetDateTime),
}

#[async_trait]
pub trait Cla: Send + Sync {
    async fn on_connect(&self, sink: Box<dyn Sink>) -> Result<()>;

    async fn on_disconnect(&self);

    async fn forward(
        &self,
        destination: &bpv7::Eid,
        addr: Option<&[u8]>,
        data: &[u8],
    ) -> Result<ForwardBundleResult>;
}

#[async_trait]
pub trait Sink: Send + Sync {
    async fn disconnect(&self);

    async fn dispatch(&self, data: &[u8]) -> Result<()>;

    async fn confirm_forwarding(&self, bundle_id: &bpv7::BundleId) -> Result<()>;

    async fn add_neighbour(
        &self,
        destination: &bpv7::Eid,
        addr: Option<&[u8]>,
        priority: u32,
    ) -> Result<()>;

    async fn remove_neighbour(&self, destination: &bpv7::Eid);
}
