use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("There is already a service using ipn service number {0}")]
    IpnServiceInUse(u32),

    #[error("There is already a service using dtn service demux {0}")]
    DtnServiceInUse(String),

    #[error("Invalid dtn service name {0}")]
    DtnInvalidServiceName(String),

    #[error("There is no ipn node id configured")]
    NoIpnNodeId,

    #[error("There is no dtn node id configured")]
    NoDtnNodeId,

    #[error("The sink is disconnected")]
    Disconnected,

    #[error("Invalid bundle destination {0}")]
    InvalidDestination(hardy_bpv7::eid::Eid),

    #[error("Bundle dropped by filter: {0:?}")]
    Dropped(Option<hardy_bpv7::status_report::ReasonCode>),

    #[error("Duplicate bundle already exists")]
    DuplicateBundle,

    #[error(transparent)]
    InvalidBundle(#[from] hardy_bpv7::Error),

    #[error(transparent)]
    Internal(#[from] Box<dyn core::error::Error + Send + Sync>),
}

pub type Result<T> = core::result::Result<T, Error>;
