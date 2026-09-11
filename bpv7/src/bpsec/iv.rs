use crate::bpsec::context::Error;

/// An RFC 9173 §4.3.1 initialization vector: 8-16 bytes by construction.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Iv {
    B8([u8; 8]),
    B9([u8; 9]),
    B10([u8; 10]),
    B11([u8; 11]),
    B12([u8; 12]),
    B13([u8; 13]),
    B14([u8; 14]),
    B15([u8; 15]),
    B16([u8; 16]),
}

impl Iv {
    /// The sole wire entry point: the only place `InvalidIvLength` can
    /// arise.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, Error> {
        macro_rules! sized {
            ($v:ident) => {
                // Infallible: the length was just matched.
                Ok(Self::$v(bytes.try_into().expect("length just matched")))
            };
        }
        match bytes.len() {
            8 => sized!(B8),
            9 => sized!(B9),
            10 => sized!(B10),
            11 => sized!(B11),
            12 => sized!(B12),
            13 => sized!(B13),
            14 => sized!(B14),
            15 => sized!(B15),
            16 => sized!(B16),
            n => Err(Error::InvalidIvLength(n)),
        }
    }

    pub fn as_slice(&self) -> &[u8] {
        match self {
            Self::B8(b) => b,
            Self::B9(b) => b,
            Self::B10(b) => b,
            Self::B11(b) => b,
            Self::B12(b) => b,
            Self::B13(b) => b,
            Self::B14(b) => b,
            Self::B15(b) => b,
            Self::B16(b) => b,
        }
    }
}
