mod admin_endpoints;
mod bundle;
mod bundle_mem;
mod cla_registry;
mod connected;
mod dispatcher;
mod fib_impl;
mod metadata_mem;
mod service_registry;
mod store;
mod utils;

pub mod bpa;
pub mod cla;
pub mod fib;
pub mod metadata;
pub mod service;
pub mod storage;

// This is the generic Error type used almost everywhere
pub type Error = Box<dyn std::error::Error + Send + Sync>;

use hardy_bpv7::prelude as bpv7;
use hardy_cbor as cbor;
use std::sync::Arc;
use trace_err::*;
use tracing::{error, info, instrument, trace, warn};

// Re-export for consistency
pub use async_trait::async_trait;
