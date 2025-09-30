/*!
# A Canonical CBOR Encoder and Decoder

This crate provides tools for encoding and decoding data in the Concise Binary
Object Representation (CBOR) format, as specified in
[RFC 8949](https://www.rfc-editor.org/rfc/rfc8949.html).

The library is designed with a focus on:
- **Canonical Encoding:** It ensures that any given data structure always
  serializes to the same, shortest possible byte representation.
- **`no_std` Compatibility:** It can be used in environments without access to
  the standard library.
- **Flexibility:** It supports both direct struct-to-CBOR conversion via traits
  and a lower-level, streaming API for more complex scenarios.

## Core Modules

The crate is split into two main modules:
- [`encode`]: Contains the `Encoder` and the `ToCbor`
  trait for serializing Rust types into CBOR.
- [`decode`]: Contains parsing functions like `parse_value` and the
  `FromCbor` trait for deserializing CBOR into Rust types.

*/
#![no_std]
extern crate alloc;

use alloc::{
    format,
    string::{String, ToString},
    vec::Vec,
};

pub mod decode;
pub mod encode;

mod decode_seq;

#[cfg(test)]
mod decode_tests;

#[cfg(test)]
mod encode_tests;
