use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    /// An attempt was made to register a CLA with a name that is already in use.
    #[error("Attempt to register duplicate CLA name {0}")]
    AlreadyExists(String),

    /// The connection to the BPA has been lost.
    #[error("The sink is disconnected")]
    Disconnected,

    /// An error occurred while processing a BPv7 bundle.
    #[error(transparent)]
    InvalidBundle(#[from] hardy_bpv7::Error),

    /// An internal error occurred.
    #[error(transparent)]
    Internal(#[from] Box<dyn core::error::Error + Send + Sync>),
}

pub type Result<T> = core::result::Result<T, Error>;
