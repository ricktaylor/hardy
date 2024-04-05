use super::*;

#[derive(Copy, Clone)]
pub enum BundleStatus {
    DispatchPending = 0,
    ForwardPending = 1,
    ReassemblyPending = 2,
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
            0 => Ok(Self::DispatchPending),
            1 => Ok(Self::ForwardPending),
            2 => Ok(Self::ReassemblyPending),
            _ => Err(anyhow!("Invalid BundleSTatus value {}", value)),
        }
    }
}
