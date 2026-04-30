//! Buffer-based CBOR codec for in-memory operations.
//!
//! Use this for small items, CRC computation, and cases that need
//! random access to encoded bytes. For streaming I/O, use
//! [`Decoder`](crate::Decoder) and [`Encoder`](crate::Encoder).

pub mod decoder;
pub mod encoder;

pub(crate) mod series;
