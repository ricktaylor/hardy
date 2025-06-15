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
