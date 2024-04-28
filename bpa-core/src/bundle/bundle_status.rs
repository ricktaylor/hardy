use super::*;

#[derive(Copy, Clone, Debug)]
pub enum BundleStatus {
    IngressPending = 0,
    DispatchPending = 1,
    ReassemblyPending = 2,
    CollectionPending = 3,
    ForwardPending = 4,
}

impl From<BundleStatus> for u64 {
    fn from(value: BundleStatus) -> Self {
        value as u64
    }
}

impl TryFrom<u64> for BundleStatus {
    type Error = anyhow::Error;

    fn try_from(value: u64) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::IngressPending),
            1 => Ok(Self::DispatchPending),
            2 => Ok(Self::ReassemblyPending),
            3 => Ok(Self::CollectionPending),
            4 => Ok(Self::ForwardPending),
            _ => Err(anyhow!("Invalid BundleStatus value {}", value)),
        }
    }
}
