use super::*;

#[derive(Copy, Clone, Debug)]
pub enum BundleStatus {
    IngressPending = 0,
    ReassemblyPending = 1,
    DispatchPending = 2,
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
            1 => Ok(Self::ReassemblyPending),
            2 => Ok(Self::DispatchPending),
            _ => Err(anyhow!("Invalid BundleStatus value {}", value)),
        }
    }
}
