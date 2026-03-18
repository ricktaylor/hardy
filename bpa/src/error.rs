use thiserror::Error;

pub type Result<T> = core::result::Result<T, Error>;

#[derive(Debug, Error)]
#[non_exhaustive]
pub enum Error {
    #[error("Node Ids must not be LocalNode")]
    LocalNode,

    #[error("Node Ids must not be the Null Endpoint")]
    NullEndpoint,

    #[error("Administrative endpoints must not have a dtn demux part")]
    DtnWithDemux,

    #[error("Multiple ipn scheme Node Ids")]
    MultipleIpnNodeIds,

    #[error("Multiple dtn scheme Node Ids")]
    MultipleDtnNodeIds,

    #[error(transparent)]
    InvalidEid(#[from] hardy_bpv7::eid::Error),

    #[error(transparent)]
    Cla(#[from] crate::cla::Error),

    #[error(transparent)]
    Services(#[from] crate::services::Error),

    #[error(transparent)]
    Filters(#[from] crate::filters::Error),

    #[error(transparent)]
    BpV7(#[from] hardy_bpv7::Error),

    #[error(transparent)]
    BpV7Editor(#[from] hardy_bpv7::editor::Error),

    #[error(transparent)]
    Internal(#[from] Box<dyn core::error::Error + Send + Sync>),
}
