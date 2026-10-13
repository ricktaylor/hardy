//! The application error codes of Section 5.

use core::fmt;

/// A Qubicle application error code, carried in QUIC CONNECTION_CLOSE, RESET_STREAM, and STOP_SENDING frames (Section 5).
///
/// The codes are one space for all three frames. A received value is read with [`Self::from_connection_close`] or [`Self::from_stream_reset`], which apply the Section 5 rules for codes that are unrecognized or out of place.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ErrorCode {
    /// `QBCL_NO_ERROR`: graceful connection closure. Meaningful only on CONNECTION_CLOSE.
    NoError,
    /// `QBCL_PROTOCOL_ERROR`: the peer violated the specification.
    ProtocolError,
    /// `QBCL_TRANSFER_CANCELLED`: a transfer aborted for a reason no other code names, such as an expired bundle lifetime.
    TransferCancelled,
    /// `QBCL_BUNDLE_TOO_LARGE`: the bundle exceeds the receiver's maximum receivable bundle size.
    BundleTooLarge,
    /// `QBCL_STORAGE_EXHAUSTED`: the receiver temporarily cannot accept bundles; the sender may retry later.
    StorageExhausted,
    /// `QBCL_SHUTTING_DOWN`: the peer is closing the session deliberately, with transfers in progress.
    ShuttingDown,
    /// `QBCL_LENGTH_REQUIRED`: the receiver does not accept direct-form bundles (Section 4.3.2).
    LengthRequired,
    /// `QBCL_UNSUPPORTED_BUNDLE_VERSION`: the receiver does not support the version of the bundle being transferred.
    UnsupportedBundleVersion,
}

impl ErrorCode {
    /// Every code, in value order.
    const ALL: [Self; 8] = [
        Self::NoError,
        Self::ProtocolError,
        Self::TransferCancelled,
        Self::BundleTooLarge,
        Self::StorageExhausted,
        Self::ShuttingDown,
        Self::LengthRequired,
        Self::UnsupportedBundleVersion,
    ];

    /// The code's value on the wire (Table 2).
    pub const fn value(self) -> u64 {
        match self {
            Self::NoError => 0x00,
            Self::ProtocolError => 0x01,
            Self::TransferCancelled => 0x02,
            Self::BundleTooLarge => 0x03,
            Self::StorageExhausted => 0x04,
            Self::ShuttingDown => 0x05,
            Self::LengthRequired => 0x06,
            Self::UnsupportedBundleVersion => 0x07,
        }
    }

    /// The code a peer's CONNECTION_CLOSE carrying `value` is treated as: an unrecognized value, including one from the Private Use range, is treated as [`Self::ProtocolError`] (Section 5).
    pub fn from_connection_close(value: u64) -> Self {
        Self::from_value(value).unwrap_or(Self::ProtocolError)
    }

    /// The code a peer's RESET_STREAM or STOP_SENDING carrying `value` is treated as: [`Self::NoError`], which a peer must not send there, and an unrecognized value are both treated as [`Self::TransferCancelled`] (Section 5).
    pub fn from_stream_reset(value: u64) -> Self {
        match Self::from_value(value) {
            Some(Self::NoError) | None => Self::TransferCancelled,
            Some(code) => code,
        }
    }

    fn from_value(value: u64) -> Option<Self> {
        Self::ALL.into_iter().find(|code| code.value() == value)
    }
}

/// Writes the code's registered name, such as `QBCL_PROTOCOL_ERROR`.
impl fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::NoError => "QBCL_NO_ERROR",
            Self::ProtocolError => "QBCL_PROTOCOL_ERROR",
            Self::TransferCancelled => "QBCL_TRANSFER_CANCELLED",
            Self::BundleTooLarge => "QBCL_BUNDLE_TOO_LARGE",
            Self::StorageExhausted => "QBCL_STORAGE_EXHAUSTED",
            Self::ShuttingDown => "QBCL_SHUTTING_DOWN",
            Self::LengthRequired => "QBCL_LENGTH_REQUIRED",
            Self::UnsupportedBundleVersion => "QBCL_UNSUPPORTED_BUNDLE_VERSION",
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The values and names of Table 2, in order.
    const TABLE_2: [(ErrorCode, u64, &str); 8] = [
        (ErrorCode::NoError, 0x00, "QBCL_NO_ERROR"),
        (ErrorCode::ProtocolError, 0x01, "QBCL_PROTOCOL_ERROR"),
        (
            ErrorCode::TransferCancelled,
            0x02,
            "QBCL_TRANSFER_CANCELLED",
        ),
        (ErrorCode::BundleTooLarge, 0x03, "QBCL_BUNDLE_TOO_LARGE"),
        (ErrorCode::StorageExhausted, 0x04, "QBCL_STORAGE_EXHAUSTED"),
        (ErrorCode::ShuttingDown, 0x05, "QBCL_SHUTTING_DOWN"),
        (ErrorCode::LengthRequired, 0x06, "QBCL_LENGTH_REQUIRED"),
        (
            ErrorCode::UnsupportedBundleVersion,
            0x07,
            "QBCL_UNSUPPORTED_BUNDLE_VERSION",
        ),
    ];

    // Values just past the assigned codes, at both ends of the Specification
    // Required range, and in the Private Use range (Section 8.3).
    const UNRECOGNIZED: [u64; 5] = [
        0x08,
        0x3FFF_FFFF,
        0x4000_0000,
        0x3FFF_FFFF_FFFF_FFFF,
        u64::MAX,
    ];

    #[test]
    fn values_and_names_match_table_2() {
        assert_eq!(ErrorCode::ALL.len(), TABLE_2.len());
        for (code, value, name) in TABLE_2 {
            assert_eq!(code.value(), value);
            assert_eq!(code.to_string(), name);
        }
    }

    #[test]
    fn a_recognized_close_code_reads_as_itself() {
        for (code, value, _) in TABLE_2 {
            assert_eq!(ErrorCode::from_connection_close(value), code);
        }
    }

    #[test]
    fn an_unrecognized_close_code_reads_as_protocol_error() {
        for value in UNRECOGNIZED {
            assert_eq!(
                ErrorCode::from_connection_close(value),
                ErrorCode::ProtocolError,
                "{value:#x}"
            );
        }
    }

    #[test]
    fn a_recognized_stream_code_other_than_no_error_reads_as_itself() {
        for (code, value, _) in TABLE_2.into_iter().skip(1) {
            assert_eq!(ErrorCode::from_stream_reset(value), code);
        }
    }

    #[test]
    fn no_error_or_an_unrecognized_stream_code_reads_as_transfer_cancelled() {
        for value in [ErrorCode::NoError.value()].into_iter().chain(UNRECOGNIZED) {
            assert_eq!(
                ErrorCode::from_stream_reset(value),
                ErrorCode::TransferCancelled,
                "{value:#x}"
            );
        }
    }
}
