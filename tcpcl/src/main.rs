mod bpa;
mod codec;
mod connection;
mod grpc;
mod listener;
mod session;
mod utils;

// Buildtime info
mod built_info {
    // The file has been placed there by the build script.
    include!(concat!(env!("OUT_DIR"), "/built.rs"));
}

// This is the effective prelude
use hardy_bpv7::prelude as bpv7;
use std::sync::Arc;
use trace_err::*;
use tracing::{error, info, instrument, trace, warn};

#[tokio::main]
async fn main() {
    // Parse command line
    let Some((config, config_source)) = utils::settings::init() else {
        return;
    };

    // Init logger
    utils::logger::init(&config);
    info!(
        "{} version {} starting...",
        built_info::PKG_NAME,
        built_info::PKG_VERSION
    );
    info!("{config_source}");

    // New BPA connection
    let mut bpa = bpa::Bpa::new(&config);

    // Prepare for graceful shutdown
    let (mut task_set, cancel_token) = utils::cancel::new_cancellable_set();

    // Init gRPC services
    grpc::init(&config, &mut task_set, cancel_token.clone());

    // Connect to the BPA
    if !cancel_token.is_cancelled() {
        bpa.connect().await;
    }

    let bpa = Arc::new(bpa);

    // Start the listener
    if !cancel_token.is_cancelled() {
        listener::init(&config, bpa.clone(), &mut task_set, cancel_token.clone());
    }

    // Wait for all tasks to finish
    if !cancel_token.is_cancelled() {
        info!("Started successfully");
    }
    while let Some(r) = task_set.join_next().await {
        r.trace_expect("Task terminated unexpectedly")
    }

    // Unregister from BPA
    bpa.disconnect().await;

    info!("Stopped");
}
