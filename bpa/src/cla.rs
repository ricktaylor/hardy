use super::*;
use thiserror::Error;

pub type Result<T> = core::result::Result<T, Error>;

#[derive(Debug, Error)]
pub enum Error {
    #[error(transparent)]
    InvalidBundle(#[from] bpv7::Error),

    #[error("The CLA is shutting down")]
    Disconnected,

    #[error(transparent)]
    Internal(#[from] Box<dyn std::error::Error + Send + Sync>),
}

pub enum ForwardBundleResult {
    Sent,
    NoNeighbour,
    TooBig(usize),
}

#[async_trait]
pub trait Cla: Send + Sync {
    async fn on_connect(&self, ident: &str, sink: Box<dyn Sink>) -> Result<()>;

    async fn on_disconnect(&self);

    async fn forward(&self, destination: &bpv7::Eid, data: &[u8]) -> Result<ForwardBundleResult>;
}

#[async_trait]
pub trait Sink: Send + Sync {
    async fn disconnect(&self);

    async fn dispatch(&self, data: &[u8]) -> Result<()>;

    async fn add_subnet(&self, pattern: eid_pattern::EidPattern);

    async fn remove_subnet(&self, pattern: &eid_pattern::EidPattern) -> bool;
}
