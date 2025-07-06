#![cfg_attr(not(feature = "std"), no_std)]
extern crate alloc;

use alloc::{
    boxed::Box,
    format,
    string::{String, ToString},
    vec::Vec,
};

// This is the problem
#[cfg(feature = "std")]
use std::collections::{HashMap, HashSet};

#[cfg(not(feature = "std"))]
use hashbrown::{HashMap, HashSet};

pub mod block;
pub mod bpsec;
pub mod builder;
pub mod bundle;
pub mod creation_timestamp;
pub mod dtn_time;
pub mod editor;
pub mod eid;
pub mod error;
pub mod hop_info;
pub mod status_report;

pub use error::Error;

mod crc;
