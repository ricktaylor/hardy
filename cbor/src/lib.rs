/*!
# hardy-cbor

RFC 8949 CBOR codec with stream-first architecture.

The primary API is the streaming codec:
- [`Decoder<R: Read>`] — reads CBOR from any byte source
- [`Encoder<W: Write>`] — writes CBOR to any byte sink

For in-memory operations (CRC computation, small items), the buffer
codec is available in the [`buffer`] module.

[RFC 8949]: https://www.rfc-editor.org/rfc/rfc8949.html
*/

#![no_std]
extern crate alloc;

mod decoder;
mod encoder;
mod error;

pub(crate) mod head;

pub mod buffer;
pub use decoder::{Decoder, UintOrString};
pub use encoder::Encoder;
pub use error::Error;
pub use hardy_io::{Read, Write};
