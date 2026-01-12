/*!
This module defines the `HopInfo` struct, which is used to track the hop limit
and hop count of a bundle as it traverses the network. This information is
typically part of the bundle's primary block and is used to prevent infinite
loops and to control the bundle's lifetime in the network.
*/

use super::*;
use error::CaptureFieldErr;

/// Contains hop limit and hop count information for a bundle.
///
/// The hop limit is the maximum number of hops a bundle is allowed to traverse,
/// while the hop count is the number of hops it has already traversed.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct HopInfo {
    /// The maximum number of hops the bundle is allowed to traverse.
    pub limit: u64,
    /// The number of hops the bundle has already traversed.
    pub count: u64,
}

impl hardy_cbor::encode::ToCbor for HopInfo {
    type Result = ();

    fn to_cbor(&self, encoder: &mut hardy_cbor::encode::Encoder) -> Self::Result {
        encoder.emit(&(&self.limit, &self.count))
    }
}

impl hardy_cbor::decode::FromCbor for HopInfo {
    type Error = Error;

    fn from_cbor(data: &[u8]) -> Result<(Self, bool, usize), Self::Error> {
        hardy_cbor::decode::parse_array(data, |a, shortest, tags| {
            let (limit, s1) = a.parse().map_field_err("hop limit")?;
            let (count, s2) = a.parse().map_field_err("hop count")?;

            Ok::<_, Error>((
                HopInfo { limit, count },
                shortest && tags.is_empty() && a.is_definite() && s1 && s2,
            ))
        })
        .map(|((v, s), len)| (v, s, len))
    }
}
