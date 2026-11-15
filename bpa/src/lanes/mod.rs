//! Lanes: how many hand-offs a CLA or service takes in flight at once.
//!
//! Queues hold waiting work; lanes carry in-flight work. A registrant
//! declares its [`Lanes`] once, at registration: a CLA in
//! [`ClaInit::lanes`](crate::cla::ClaInit), a service or application as the
//! `lanes` argument of its registration. The BPA keeps at most that many
//! hand-offs in flight to it, each holding its loaded bundle, further
//! bounded by the BPA's own seat cap
//! ([`BpaBuilder::egress_lane_cap`](crate::builder::BpaBuilder::egress_lane_cap)
//! for a CLA's peers,
//! [`BpaBuilder::delivery_lane_cap`](crate::builder::BpaBuilder::delivery_lane_cap)
//! for a service). Concurrent hand-offs may complete in any order.

use core::num::NonZeroU32;

pub(crate) mod seat;

/// A registrant's declared lanes: its honest parallelism, how many
/// hand-offs it can usefully take in flight at once.
///
/// For a CLA a hand-off is a forward to one peer, and its lanes are its
/// parallel transport channels — QUIC streams, DSCP classes, separate TCP
/// connections — carrying in-flight transfers with no explicit priority
/// among them: the BPA's egress policy decides what goes onto each lane,
/// and one lane's in-flight transfer must not head-of-line block another's.
/// For a service or application a hand-off is a delivery, a concurrent
/// `on_deliver` call.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Lanes {
    /// One hand-off at a time, in queue order. A registrant that declares
    /// nothing gets this.
    #[default]
    Serial,
    /// Up to this many hand-offs at once; a zero count is unrepresentable
    /// by construction. A CLA's lanes are indexed `0..n` on each forward.
    Limited(NonZeroU32),
    /// No limit of the registrant's own (for a CLA, a datagram CL, where
    /// every forward travels on a fresh lane). The BPA's own bounds still
    /// apply.
    Unlimited,
}

impl Lanes {
    // How many explicit lanes the declaration names: `n` for `Limited(n)`,
    // none otherwise, since serial and unlimited registrants index no lanes.
    pub(crate) fn explicit(self) -> u32 {
        match self {
            Self::Limited(n) => n.get(),
            Self::Serial | Self::Unlimited => 0,
        }
    }
}
