/// The EgressPolicy enum now cleanly separates the type from its configuration.
#[derive(Debug)]
#[cfg_attr(feature = "serde", derive(serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(tag = "type", rename_all = "lowercase"))]
pub enum EgressPolicyConfig {
    // #[cfg(feature = "htb_policy")]
    // Htb { config: htb_policy::HtbConfig },

    // #[cfg(feature = "tbf_policy")]
    // Tbf { config: tbf_policy::TbfConfig },
}
