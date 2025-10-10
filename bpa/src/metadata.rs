use super::*;

#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "bincode", derive(bincode::Encode, bincode::Decode))]
pub enum BundleStatus {
    Dispatching,
    ForwardPending { peer: u32, queue: Option<u32> },
    LocalPending { service: u32 },
    Waiting,
}

#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct BundleMetadata {
    pub(crate) storage_name: Option<Arc<str>>,

    pub status: BundleStatus,
    pub received_at: time::OffsetDateTime,
    pub non_canonical: bool,
    pub flow_label: Option<u32>,
}

impl Default for BundleMetadata {
    fn default() -> Self {
        Self {
            storage_name: None,
            status: BundleStatus::Dispatching,
            received_at: time::OffsetDateTime::now_utc(),
            non_canonical: false,
            flow_label: None,
        }
    }
}

#[cfg(feature = "bincode")]
impl bincode::Encode for BundleMetadata {
    fn encode<E: bincode::enc::Encoder>(
        &self,
        encoder: &mut E,
    ) -> Result<(), bincode::error::EncodeError> {
        bincode::Encode::encode(&self.status, encoder)?;
        bincode::Encode::encode(&self.storage_name, encoder)?;
        bincode::Encode::encode(&self.received_at.unix_timestamp_nanos(), encoder)?;
        bincode::Encode::encode(&self.non_canonical, encoder)?;
        bincode::Encode::encode(&self.flow_label, encoder)?;
        Ok(())
    }
}

#[cfg(feature = "bincode")]
impl<Context> bincode::Decode<Context> for BundleMetadata {
    fn decode<D: bincode::de::Decoder<Context = Context>>(
        decoder: &mut D,
    ) -> Result<Self, bincode::error::DecodeError> {
        Ok(Self {
            status: bincode::Decode::decode(decoder)?,
            storage_name: bincode::Decode::decode(decoder)?,
            received_at: {
                time::OffsetDateTime::from_unix_timestamp_nanos(bincode::Decode::decode(decoder)?)
                    .map_err(|_| bincode::error::DecodeError::Other("bad timestamp"))?
            },
            non_canonical: bincode::Decode::decode(decoder)?,
            flow_label: bincode::Decode::decode(decoder)?,
        })
    }
}

#[cfg(feature = "bincode")]
impl<'de, Context> bincode::BorrowDecode<'de, Context> for BundleMetadata {
    fn borrow_decode<D: bincode::de::BorrowDecoder<'de, Context = Context>>(
        decoder: &mut D,
    ) -> core::result::Result<Self, bincode::error::DecodeError> {
        Ok(Self {
            status: bincode::BorrowDecode::borrow_decode(decoder)?,
            storage_name: bincode::BorrowDecode::borrow_decode(decoder)?,
            received_at: {
                time::OffsetDateTime::from_unix_timestamp_nanos(
                    bincode::BorrowDecode::borrow_decode(decoder)?,
                )
                .map_err(|_| bincode::error::DecodeError::Other("bad timestamp"))?
            },
            non_canonical: bincode::BorrowDecode::borrow_decode(decoder)?,
            flow_label: bincode::BorrowDecode::borrow_decode(decoder)?,
        })
    }
}
