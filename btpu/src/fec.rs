//! FEC extension message types and framework trait (draft-ietf-dtn-btpu-fec-01).
//!
//! This module defines the four FEC message types introduced by the BTP-U FEC
//! extension, plus the [`FecScheme`] trait that a pluggable FEC codec must
//! implement.  No concrete FEC scheme implementations are provided.

use crate::hint::HintItem;
use alloc::vec::Vec;
use bytes::Bytes;

/// Pre-agreed FEC Source message (type 0x70).
///
/// Carries a single Application Data Unit (ADU) for a transfer using a
/// pre-configured FEC scheme identified by `fec_instance_id`.
#[derive(Debug, Clone)]
pub struct PreAgreedFecSourceMessage {
    pub transfer_number: u32,
    pub fec_instance_id: u8,
    pub hints: Vec<HintItem>,
    /// Source FEC Payload ID followed by the source ADU data, as raw bytes.
    ///
    /// The boundary between the two is scheme-defined and cannot be
    /// determined without the pre-agreed scheme: split at
    /// [`FecScheme::source_fec_payload_id_size`].
    pub payload: Bytes,
}

/// Explicit FEC Source message (type 0x71).
///
/// Carries a single ADU with inline FEC-Scheme-Specific Information (FSSI).
#[derive(Debug, Clone)]
pub struct ExplicitFecSourceMessage {
    pub transfer_number: u32,
    pub fec_encoding_id: u8,
    pub hints: Vec<HintItem>,
    /// FSSI, then the Source FEC Payload ID, then the source ADU data, as
    /// raw bytes.
    ///
    /// The boundaries are scheme-defined: split at the length of
    /// [`FecScheme::fssi`] and then [`FecScheme::source_fec_payload_id_size`]
    /// for the scheme identified by `fec_encoding_id`.
    pub payload: Bytes,
}

/// Pre-agreed FEC Repair message (type 0x72).
///
/// Carries repair symbols for a transfer using a pre-configured FEC scheme.
#[derive(Debug, Clone)]
pub struct PreAgreedFecRepairMessage {
    pub transfer_number: u32,
    pub fec_instance_id: u8,
    pub hints: Vec<HintItem>,
    /// Repair FEC Payload ID followed by the repair symbols, as raw bytes.
    ///
    /// The boundary between the two is scheme-defined: split at
    /// [`FecScheme::repair_fec_payload_id_size`].
    pub payload: Bytes,
}

/// Explicit FEC Repair message (type 0x73).
///
/// Carries repair symbols with inline FSSI.
#[derive(Debug, Clone)]
pub struct ExplicitFecRepairMessage {
    pub transfer_number: u32,
    pub fec_encoding_id: u8,
    pub hints: Vec<HintItem>,
    /// FSSI, then the Repair FEC Payload ID, then the repair symbols, as raw
    /// bytes.
    ///
    /// The boundaries are scheme-defined: split at the length of
    /// [`FecScheme::fssi`] and then [`FecScheme::repair_fec_payload_id_size`]
    /// for the scheme identified by `fec_encoding_id`.
    pub payload: Bytes,
}

/// Trait for a pluggable FEC scheme following the FECFRAME framework (RFC 6363).
///
/// Implementors provide encoding and decoding for a specific FEC algorithm
/// (e.g., Reed-Solomon, RaptorQ).  The BTP-U library uses this trait to:
///
/// - Determine the sizes of FEC Payload IDs for message parsing.
/// - (In a future CLA) Encode bundles into source ADUs and repair symbols.
/// - (In a future CLA) Reconstruct bundles from received source and repair data.
pub trait FecScheme: Send + Sync {
    /// The FEC Encoding ID for this scheme (RFC 6363 Section 5.6).
    fn encoding_id(&self) -> u8;

    /// Size in bytes of the Explicit Source FEC Payload ID produced by this
    /// scheme.
    fn source_fec_payload_id_size(&self) -> usize;

    /// Size in bytes of the Repair FEC Payload ID produced by this scheme.
    fn repair_fec_payload_id_size(&self) -> usize;

    /// The FEC-Scheme-Specific Information (FSSI) bytes for this scheme.
    fn fssi(&self) -> Bytes;

    /// Divide `bundle_data` into ADUs of at most `adu_size` bytes and produce
    /// `(source_fec_payload_id, adu_data)` pairs.
    fn encode_source(&self, bundle_data: &[u8], adu_size: usize) -> Vec<(Bytes, Bytes)>;

    /// Generate `num_repair` repair symbols from the given source ADUs.
    /// Returns `(repair_fec_payload_id, repair_data)` pairs.
    fn generate_repair(
        &self,
        source_adus: &[(Bytes, Bytes)],
        num_repair: usize,
    ) -> Vec<(Bytes, Bytes)>;

    /// Attempt to reconstruct the original bundle from received source ADUs
    /// and repair symbols.  Returns `None` if insufficient data.
    fn decode(
        &self,
        source_adus: &[(Bytes, Bytes)],
        repair_symbols: &[(Bytes, Bytes)],
        bundle_length: Option<u64>,
    ) -> Option<Bytes>;
}
