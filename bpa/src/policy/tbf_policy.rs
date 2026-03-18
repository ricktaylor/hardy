/// Configuration for the TBF (Token Bucket Filter) policy.
#[derive(Debug)]
#[cfg_attr(feature = "serde", derive(serde::Deserialize))]
pub struct TbfConfig {
    pub rate: String,
    pub burst: String,
    pub latency: String,
}
