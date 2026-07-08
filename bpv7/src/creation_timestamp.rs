/*!
This module defines the `CreationTimestamp`, a critical component of a bundle's unique identification.
As per RFC 9171, it combines a timestamp with a sequence number to ensure that each bundle from a
given source node can be uniquely identified, even if created at the same time.
*/

use super::*;
use error::{CaptureFieldErr, require_canonical};

static GLOBAL_COUNTER: portable_atomic::AtomicU64 = portable_atomic::AtomicU64::new(1);

/// Represents the BPv7 Creation Timestamp, a tuple of creation time and a sequence number.
///
/// As defined in RFC 9171, the creation timestamp is a tuple `[time, sequence]`.
/// The `time` is a DTN Time, which is the number of non-leap milliseconds since the
/// DTN epoch (2000-01-01 00:00:00 UTC). If a node does not have an accurate clock,
/// this value is set to 0.
/// The `sequence` number is a sequence number that is larger than the sequence number
/// of any previously transmitted bundle from the same node.
#[derive(Default, Debug, Clone, Hash, PartialEq, Eq, PartialOrd, Ord)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct CreationTimestamp {
    /// The time the bundle was created. `None` if the source node does not have an accurate clock.
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "Option::is_none")
    )]
    creation_time: Option<dtn_time::DtnTime>,
    /// A sequence number that is unique for the source node.
    sequence_number: u64,
}

impl CreationTimestamp {
    /// Creates a new `CreationTimestamp` based on the current system time.
    ///
    /// The creation time is set to the current UTC time. The sequence number
    /// is derived from the nanoseconds part of the timestamp to provide uniqueness
    /// for bundles created in the same millisecond.
    ///
    /// This function is only available when the `std` feature is enabled.
    #[cfg(feature = "std")]
    pub fn now() -> Self {
        let timestamp = time::OffsetDateTime::now_utc();
        Self {
            sequence_number: (timestamp.nanosecond() % 1_000_000) as u64,
            creation_time: Some(dtn_time::DtnTime::saturating_from(timestamp)),
        }
    }

    /// Creates a new `CreationTimestamp` without a time value.
    ///
    /// The creation time is set to `None`, indicating the absence of an accurate clock.
    /// The sequence number is generated from a globally unique atomic counter.
    ///
    /// This function is only available when the `std` feature is *not* enabled.
    pub fn new_sequential() -> Self {
        Self {
            creation_time: None,
            sequence_number: GLOBAL_COUNTER.fetch_add(1, portable_atomic::Ordering::Relaxed),
        }
    }

    /// Create a new `CreationTimestamp` with the given time and sequence
    /// number values.
    pub fn from_parts(creation_time: Option<dtn_time::DtnTime>, sequence_number: u64) -> Self {
        Self {
            creation_time,
            sequence_number,
        }
    }

    /// Disassembles the `CreationTimestamp` into its parts, the time value
    /// and the sequence number.
    pub fn into_parts(self) -> (Option<dtn_time::DtnTime>, u64) {
        let Self {
            creation_time,
            sequence_number,
        } = self;
        (creation_time, sequence_number)
    }

    /// Access the creation_time value of this timestamp.
    pub fn creation_time(&self) -> Option<&dtn_time::DtnTime> {
        self.creation_time.as_ref()
    }

    /// Access the sequence number of this timestamp.
    pub fn sequence_number(&self) -> u64 {
        self.sequence_number
    }

    /// Returns `true` if the `CreationTimestamp` was created by a source with an accurate clock.
    ///
    /// This is determined by the presence of a `creation_time` value.
    pub fn is_clocked(&self) -> bool {
        self.creation_time.is_some()
    }

    /// Converts the `CreationTimestamp` to a `time::OffsetDateTime`, if possible.
    ///
    /// Returns `Some(OffsetDateTime)` if the `creation_time` is present, nudged
    /// forward by the sequence number for sub-millisecond ordering. Returns
    /// `None` if the `creation_time` is not set.
    ///
    /// The sequence number is an unrestricted `u64` on the wire and need not be
    /// nanoseconds — [`CreationTimestamp::now`] uses the sub-millisecond
    /// nanosecond remainder, but other implementations use plain counters. The
    /// nudge is therefore clamped below the millisecond resolution of
    /// [`DtnTime`](dtn_time::DtnTime), so a sender-chosen sequence number
    /// cannot shift the result outside the creation millisecond.
    pub fn as_datetime(&self) -> Option<time::OffsetDateTime> {
        let t: time::OffsetDateTime = self.creation_time?.into();
        Some(t.saturating_add(time::Duration::nanoseconds(
            self.sequence_number.min(999_999) as i64,
        )))
    }
}

impl core::fmt::Display for CreationTimestamp {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        if let Some(ct) = self.creation_time {
            write!(f, "{} seq {}", ct, self.sequence_number)
        } else {
            write!(f, "(No clock) {}", self.sequence_number)
        }
    }
}

impl hardy_cbor::encode::ToCbor for CreationTimestamp {
    type Result = ();

    fn to_cbor(&self, encoder: &mut hardy_cbor::encode::Encoder) -> Self::Result {
        encoder.emit(&(
            &self.creation_time.unwrap_or_default(),
            &self.sequence_number,
        ));
    }
}

impl hardy_cbor::decode::FromCbor for CreationTimestamp {
    type Error = Error;

    /// Strict-canonical decode per RFC 9171 §4.1: non-shortest array
    /// head, non-shortest sub-field encoding, and unexpected tags are
    /// rejected with `NotCanonical`. Indefinite-length array encoding
    /// is accepted (§4.1 carveout) and reflected in the returned
    /// `shortest` flag as `false`.
    fn from_cbor(data: &[u8]) -> Result<(Self, bool, usize), Self::Error> {
        hardy_cbor::decode::parse_array(data, |a, shortest, tags| {
            if !shortest || !tags.is_empty() {
                return Err(Error::NotCanonical);
            }
            // `DtnTime` self-enforces canonical form, so delegate to it
            // rather than re-checking via `require_canonical`.
            let creation_time: dtn_time::DtnTime =
                a.parse().map_field_err::<Error>("bundle creation time")?;
            let sequence_number = require_canonical(a, "sequence number")?;
            Ok((
                CreationTimestamp {
                    // A zero DTN time signals the source had no usable clock.
                    creation_time: if creation_time.millisecs() == 0 {
                        None
                    } else {
                        Some(creation_time)
                    },
                    sequence_number,
                },
                a.is_definite(),
            ))
        })
        .map(|((v, s), len)| (v, s, len))
    }
}

impl TryFrom<time::OffsetDateTime> for CreationTimestamp {
    type Error = <dtn_time::DtnTime as TryFrom<time::OffsetDateTime>>::Error;

    fn try_from(value: time::OffsetDateTime) -> Result<Self, Self::Error> {
        Ok(Self {
            creation_time: Some(value.try_into()?),
            sequence_number: (value.nanosecond() % 1_000_000) as u64,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clocked(sequence_number: u64) -> CreationTimestamp {
        CreationTimestamp {
            creation_time: Some(dtn_time::DtnTime::new(820_000_000_000)),
            sequence_number,
        }
    }

    #[test]
    fn as_datetime_applies_sub_millisecond_nudge() {
        let base = clocked(0).as_datetime().unwrap();
        let nudged = clocked(500).as_datetime().unwrap();
        assert_eq!(nudged - base, time::Duration::nanoseconds(500));
    }

    // Wire sequence numbers are unrestricted u64: values at or above 2^63
    // must not wrap to a negative nanosecond offset, and large positive
    // values must not shift the time by centuries. Both clamp to less than
    // one millisecond.
    #[test]
    fn as_datetime_clamps_wire_range_sequence_numbers() {
        let base = clocked(0).as_datetime().unwrap();
        for seq in [1_000_000, i64::MAX as u64, 1 << 63, u64::MAX] {
            let nudged = clocked(seq).as_datetime().unwrap();
            let delta = nudged - base;
            assert!(
                delta >= time::Duration::ZERO && delta < time::Duration::MILLISECOND,
                "sequence {seq} produced out-of-millisecond delta {delta}"
            );
        }
    }
}
