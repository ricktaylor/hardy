//! Portable I/O traits.
//!
//! [`Read`] and [`Write`] abstract over `std::io` (with `std` feature)
//! and `embedded-io` (without `std`). All streaming code is written
//! against these traits.

#![cfg_attr(not(feature = "std"), no_std)]
extern crate alloc;

mod error;
mod traits;

pub use error::Error;
pub use traits::{Read, Write};
