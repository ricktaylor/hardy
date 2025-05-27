use super::*;
use thiserror::Error;

pub type Result<T> = core::result::Result<T, Error>;

#[derive(Debug, Error)]
pub enum Error {
    #[error("Attempt to register duplicate CLA name {0}")]
    AlreadyExists(String),

    #[error("The sink is disconnected")]
    Disconnected,

    #[error(transparent)]
    InvalidBundle(#[from] bpv7::Error),

    #[error(transparent)]
    Internal(#[from] Box<dyn std::error::Error + Send + Sync>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ClaAddressType {
    TcpClv4,
    Unknown(u32),
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ClaAddress {
    TcpClv4Address(core::net::SocketAddr),
    Unknown(u32, Bytes),
}

impl ClaAddress {
    pub fn address_type(&self) -> ClaAddressType {
        match self {
            ClaAddress::TcpClv4Address(_) => ClaAddressType::TcpClv4,
            ClaAddress::Unknown(t, _) => ClaAddressType::Unknown(*t),
        }
    }
}

impl TryFrom<(ClaAddressType, Bytes)> for ClaAddress {
    type Error = Error;

    fn try_from((addr_type, addr): (ClaAddressType, Bytes)) -> Result<Self> {
        match addr_type {
            ClaAddressType::TcpClv4 => Ok(ClaAddress::TcpClv4Address(
                String::from_utf8(addr.into())
                    .map_err(|e| Error::Internal(Box::new(e)))?
                    .parse()
                    .map_err(|e| Error::Internal(Box::new(e)))?,
            )),
            ClaAddressType::Unknown(s) => Ok(ClaAddress::Unknown(s, addr)),
        }
    }
}

impl From<ClaAddress> for (ClaAddressType, Bytes) {
    fn from(value: ClaAddress) -> Self {
        match value {
            ClaAddress::TcpClv4Address(socket_addr) => (
                ClaAddressType::TcpClv4,
                socket_addr.to_string().as_bytes().to_vec().into(),
            ),
            ClaAddress::Unknown(t, bytes) => (ClaAddressType::Unknown(t), bytes),
        }
    }
}

pub enum ForwardBundleResult {
    Sent,
    NoNeighbour,
    TooBig(u64),
}

#[async_trait]
pub trait Cla: Send + Sync {
    async fn on_register(&self, sink: Box<dyn Sink>);

    async fn on_unregister(&self);

    async fn on_forward(&self, cla_addr: ClaAddress, bundle: Bytes) -> Result<ForwardBundleResult>;
}

#[async_trait]
pub trait Sink: Send + Sync {
    async fn unregister(&self);

    async fn dispatch(&self, bundle: Bytes) -> Result<()>;

    async fn add_peer(&self, eid: bpv7::Eid, addr: ClaAddress) -> Result<()>;

    async fn remove_peer(&self, eid: &bpv7::Eid) -> Result<bool>;
}
