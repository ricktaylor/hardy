pub mod app_registry;
pub mod cla_registry;
pub mod dispatcher;
pub mod fib;
pub mod grpc;
pub mod static_routes;
pub mod store;
pub mod utils;

// This is the generic Error type used almost everywhere
type Error = Box<dyn std::error::Error + Send + Sync>;

// This is the effective prelude
use fuzz_macros::instrument;
use hardy_bpa_api::metadata;
use hardy_bpv7::prelude as bpv7;
use trace_err::*;
use tracing::{error, info, trace, warn};
