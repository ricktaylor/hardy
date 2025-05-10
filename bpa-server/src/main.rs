mod config;
mod static_routes;

// This is the generic Error type used almost everywhere
type Error = Box<dyn std::error::Error + Send + Sync>;

use std::sync::Arc;
use trace_err::*;
use tracing::{error, info, trace};

fn listen_for_cancel(
    task_set: &mut tokio::task::JoinSet<()>,
    cancel_token: tokio_util::sync::CancellationToken,
) {
    cfg_if::cfg_if! {
        if #[cfg(unix)] {
            let mut term_handler =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                .trace_expect("Failed to register signal handlers");
        } else {
            let mut term_handler = std::future::pending();
        }
    }
    task_set.spawn(async move {
        tokio::select! {
            _ = term_handler.recv() => {
                // Signal stop
                info!("Received terminate signal, stopping...");
            }
            _ = tokio::signal::ctrl_c() => {
                // Signal stop
                info!("Received CTRL+C, stopping...");
            }
        }

        // Cancel everything
        cancel_token.cancel();
    });
}

#[tokio::main]
async fn main() {
    // Parse command line
    let Some(config) = config::init() else {
        return;
    };

    // Start the BPA
    let bpa = Arc::new(hardy_bpa::bpa::Bpa::start(&config.bpa).await);

    // Prepare for graceful shutdown
    let cancel_token = tokio_util::sync::CancellationToken::new();
    let mut task_set = tokio::task::JoinSet::new();

    // Load static routes
    if let Some(config) = config.static_routes {
        static_routes::init(config, bpa.clone(), &mut task_set, cancel_token.clone()).await;
    }

    // And wait for shutdown signal
    listen_for_cancel(&mut task_set, cancel_token.clone());

    info!("Started successfully");

    // And wait for cancel token
    cancel_token.cancelled().await;

    // Shut down bpa
    bpa.shutdown().await;

    // Wait for all tasks to finish
    while let Some(r) = task_set.join_next().await {
        r.trace_expect("Task terminated unexpectedly")
    }

    info!("Stopped");
}
