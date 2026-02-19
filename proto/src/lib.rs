// Because Prost is too lose with Rustdoc comments
#![allow(clippy::doc_lazy_continuation)]

use hardy_bpa::async_trait;
use hardy_bpv7::eid;
use std::sync::{Arc, Weak};
use tracing::{debug, error, info, warn};

pub(crate) mod proto;
pub(crate) mod proxy;

pub mod client;
pub mod server;
