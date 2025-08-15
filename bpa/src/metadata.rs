use super::*;

#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct BundleMetadata {
    pub storage_name: Option<Arc<str>>,
    pub received_at: time::OffsetDateTime,
}

#[cfg(feature = "bincode")]
impl bincode::Encode for BundleMetadata {
    fn encode<E: bincode::enc::Encoder>(
        &self,
        encoder: &mut E,
    ) -> Result<(), bincode::error::EncodeError> {
        bincode::Encode::encode(&self.storage_name, encoder)?;
        bincode::Encode::encode(&self.received_at.unix_timestamp_nanos(), encoder)?;
        Ok(())
    }
}

#[cfg(feature = "bincode")]
impl<Context> bincode::Decode<Context> for BundleMetadata {
    fn decode<D: bincode::de::Decoder<Context = Context>>(
        decoder: &mut D,
    ) -> Result<Self, bincode::error::DecodeError> {
        Ok(Self {
            storage_name: bincode::Decode::decode(decoder)?,
            received_at: {
                time::OffsetDateTime::from_unix_timestamp_nanos(bincode::Decode::decode(decoder)?)
                    .map_err(|_| bincode::error::DecodeError::Other("bad timestamp"))?
            },
        })
    }
}

#[cfg(feature = "bincode")]
impl<'de, Context> bincode::BorrowDecode<'de, Context> for BundleMetadata {
    fn borrow_decode<D: bincode::de::BorrowDecoder<'de, Context = Context>>(
        decoder: &mut D,
    ) -> core::result::Result<Self, bincode::error::DecodeError> {
        Ok(Self {
            storage_name: bincode::BorrowDecode::borrow_decode(decoder)?,
            received_at: {
                time::OffsetDateTime::from_unix_timestamp_nanos(
                    bincode::BorrowDecode::borrow_decode(decoder)?,
                )
                .map_err(|_| bincode::error::DecodeError::Other("bad timestamp"))?
            },
        })
    }
}
