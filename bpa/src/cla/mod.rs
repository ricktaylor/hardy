use super::*;
use thiserror::Error;

pub mod policy;
pub mod registry;

// #[cfg(feature = "htb_policy")]
// pub mod htb_policy;

// #[cfg(feature = "tbf_policy")]
// pub mod tbf_policy;

mod peers;

pub type Result<T> = core::result::Result<T, Error>;

#[derive(Debug, Error)]
pub enum Error {
    #[error("Attempt to register duplicate CLA name {0}")]
    AlreadyExists(String),

    #[error("The sink is disconnected")]
    Disconnected,

    #[error("The CLA is already connected")]
    AlreadyConnected,

    #[error(transparent)]
    InvalidBundle(#[from] hardy_bpv7::Error),

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

impl std::fmt::Display for ClaAddress {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ClaAddress::TcpClv4Address(socket_addr) => write!(f, "{socket_addr}"),
            ClaAddress::Unknown(t, bytes) => write!(f, "raw({t}):{bytes:?}"),
        }
    }
}

pub enum ForwardBundleResult {
    Sent,
    NoNeighbour,
    TooBig(u64),
}

#[async_trait]
pub trait EgressController: Send + Sync {
    async fn forward(
        &self,
        queue: u32,
        cla_addr: ClaAddress,
        bundle: Bytes,
    ) -> Result<ForwardBundleResult>;
}

#[async_trait]
pub trait Cla: EgressController {
    async fn on_register(
        &self,
        sink: Box<dyn Sink>,
        node_ids: &[hardy_bpv7::eid::Eid],
    ) -> Result<()>;

    async fn on_unregister(&self);
}

#[async_trait]
pub trait EgressPolicy: Send + Sync {
    fn queue_count(&self) -> u32 {
        1
    }

    fn classify(&self, flow_label: u32) -> u32;

    async fn new_controller(&self, cla: Arc<dyn Cla>) -> Arc<dyn EgressController>;
}

#[async_trait]
pub trait Sink: Send + Sync {
    async fn unregister(&self);

    async fn dispatch(&self, bundle: Bytes) -> Result<()>;

    async fn add_peer(&self, eid: hardy_bpv7::eid::Eid, addr: ClaAddress) -> Result<bool>;

    async fn remove_peer(&self, eid: &hardy_bpv7::eid::Eid, addr: &ClaAddress) -> Result<bool>;
}
