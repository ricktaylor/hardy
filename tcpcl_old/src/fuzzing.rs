// This file is only used for fuzzing

pub mod listener;
pub mod utils;

mod codec;
mod connection;
mod session;

use fuzz_macros::instrument;
use hardy_bpv7::prelude as bpv7;
use trace_err::*;
use tracing::{error, info, trace, warn};

// Buildtime info
mod built_info {
    // The file has been placed there by the build script.
    include!(concat!(env!("OUT_DIR"), "/built.rs"));
}

// Mock BPA
pub mod bpa {
    #[derive(Clone)]
    pub struct Bpa {}

    impl Bpa {
        pub fn new(_config: &config::Config) -> Self {
            Self {}
        }

        pub async fn send(&self, _bundle: tokio_util::bytes::Bytes) -> Result<(), tonic::Status> {
            Ok(())
        }
    }
}

mod grpc {
    pub fn to_timestamp(t: time::OffsetDateTime) -> prost_types::Timestamp {
        let t = t - time::OffsetDateTime::UNIX_EPOCH;
        prost_types::Timestamp {
            seconds: t.whole_seconds(),
            nanos: t.subsec_nanoseconds(),
        }
    }
}
