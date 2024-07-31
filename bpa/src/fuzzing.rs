pub mod app_registry;
pub mod cla_registry;
pub mod dispatcher;
pub mod fib;
pub mod ingress;
pub mod store;
pub mod utils;

// This is the generic Error type used almost everywhere
type Error = Box<dyn std::error::Error + Send + Sync>;

// Buildtime info
mod built_info {
    // The file has been placed there by the build script.
    include!(concat!(env!("OUT_DIR"), "/built.rs"));
}

// This is the effective prelude
use fuzz_macros::instrument;
use hardy_bpa_api::metadata;
use hardy_bpv7::prelude as bpv7;
use trace_err::*;
use tracing::{error, info, trace, warn};

mod services {
    use super::*;

    pub fn from_timestamp(t: prost_types::Timestamp) -> Result<time::OffsetDateTime, Error> {
        Ok(time::OffsetDateTime::from_unix_timestamp(t.seconds)
            .map_err(time::error::ComponentRange::from)?
            + time::Duration::nanoseconds(t.nanos.into()))
    }

    pub fn to_timestamp(t: time::OffsetDateTime) -> prost_types::Timestamp {
        let t = t - time::OffsetDateTime::UNIX_EPOCH;
        prost_types::Timestamp {
            seconds: t.whole_seconds(),
            nanos: t.subsec_nanoseconds(),
        }
    }
}
