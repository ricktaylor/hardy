use tokio_util::bytes::{Buf, BufMut, Bytes, BytesMut};

/// MTCP codec: CBOR byte string framing (draft-ietf-dtn-mtcpcl-01).
///
/// Each bundle is encoded as a CBOR byte string (major type 2):
/// - 1-byte header for lengths 0-23
/// - 2-byte header (0x58 + u8) for lengths 24-255
/// - 3-byte header (0x59 + u16 BE) for lengths 256-65535
/// - 5-byte header (0x5a + u32 BE) for lengths 65536-4294967295
/// - 9-byte header (0x5b + u64 BE) for lengths > 4294967295
pub struct MtcpCodec {
    max_bundle_size: u64,
    /// Bundle length parsed from header, waiting for payload bytes.
    pending_length: Option<usize>,
}

impl MtcpCodec {
    pub fn new(max_bundle_size: u64) -> Self {
        Self {
            max_bundle_size,
            pending_length: None,
        }
    }
}

impl tokio_util::codec::Decoder for MtcpCodec {
    type Item = Bytes;
    type Error = std::io::Error;

    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
        // If we already parsed the header, wait for payload
        if let Some(length) = self.pending_length {
            if src.len() < length {
                return Ok(None);
            }
            self.pending_length = None;
            return Ok(Some(src.split_to(length).freeze()));
        }

        // Need at least 1 byte for the CBOR header
        if src.is_empty() {
            return Ok(None);
        }

        // Parse CBOR byte string header using hardy-cbor's pull parser
        match hardy_cbor::decode::parse_value(src, |value, _shortest, tags| {
            if !tags.is_empty() {
                return Err(hardy_cbor::decode::Error::IncorrectType(
                    "Untagged Byte String".into(),
                    "Tagged value".into(),
                ));
            }
            match value {
                hardy_cbor::decode::Value::Bytes(range) => Ok(range),
                other => Err(hardy_cbor::decode::Error::IncorrectType(
                    "Byte String".into(),
                    other.type_name(false),
                )),
            }
        }) {
            Ok((range, consumed)) => {
                if self.max_bundle_size > 0 && range.len() as u64 > self.max_bundle_size {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!(
                            "MTCP bundle length {} exceeds maximum {}",
                            range.len(),
                            self.max_bundle_size
                        ),
                    ));
                }
                // The entire CBOR byte string (header + payload) was in the buffer.
                // Extract just the payload bytes.
                let payload = Bytes::copy_from_slice(&src[range]);
                src.advance(consumed);
                Ok(Some(payload))
            }
            Err(hardy_cbor::decode::Error::NeedMoreData(_)) => {
                // Not enough data yet — the CBOR header or payload is incomplete.
                Ok(None)
            }
            Err(e) => Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("MTCP CBOR decode error: {e}"),
            )),
        }
    }
}

impl tokio_util::codec::Encoder<Bytes> for MtcpCodec {
    type Error = std::io::Error;

    fn encode(&mut self, item: Bytes, dst: &mut BytesMut) -> Result<(), Self::Error> {
        // Write only the CBOR byte string header (major type 2 + length),
        // then the raw payload bytes.
        let (header, _) = hardy_cbor::encode::emit(&hardy_cbor::encode::BytesHeader(&item));
        dst.extend_from_slice(&header);
        dst.put(item);
        Ok(())
    }
}

/// STCP codec: 4-byte big-endian u32 length prefix (ION wire format).
///
/// Each bundle is preceded by a 4-byte network-order length.
/// A zero-length preamble is a keepalive (skipped on decode).
pub struct StcpCodec {
    max_bundle_size: u64,
}

impl StcpCodec {
    pub fn new(max_bundle_size: u64) -> Self {
        Self { max_bundle_size }
    }
}

impl tokio_util::codec::Decoder for StcpCodec {
    type Item = Bytes;
    type Error = std::io::Error;

    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
        loop {
            if src.len() < 4 {
                return Ok(None);
            }

            let length = u32::from_be_bytes([src[0], src[1], src[2], src[3]]) as usize;

            // Zero-length preamble = ION keepalive, skip it
            if length == 0 {
                src.advance(4);
                continue;
            }

            if length as u64 > self.max_bundle_size {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!(
                        "STCP bundle length {length} exceeds maximum {}",
                        self.max_bundle_size
                    ),
                ));
            }

            if src.len() < 4 + length {
                // Reserve space to avoid repeated allocations
                src.reserve(4 + length - src.len());
                return Ok(None);
            }

            src.advance(4); // consume length prefix
            return Ok(Some(src.split_to(length).freeze()));
        }
    }
}

impl tokio_util::codec::Encoder<Bytes> for StcpCodec {
    type Error = std::io::Error;

    fn encode(&mut self, item: Bytes, dst: &mut BytesMut) -> Result<(), Self::Error> {
        let length: u32 = item.len().try_into().map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("Bundle too large for STCP: {} bytes", item.len()),
            )
        })?;
        dst.put_u32(length);
        dst.put(item);
        Ok(())
    }
}
