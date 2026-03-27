//! Signal handling for graceful shutdown.
//!
//! Provides [`listen_for_cancel`] which spawns a task that listens for
//! SIGTERM and Ctrl+C, cancelling the provided [`TaskPool`] when received.
//! This is the standard Hardy shutdown pattern used by all server binaries.
//!
//! # Example
//!
//! ```no_run
//! use hardy_async::TaskPool;
//!
//! # tokio::runtime::Runtime::new().unwrap().block_on(async {
//! let tasks = TaskPool::new();
//! hardy_async::signal::listen_for_cancel(&tasks);
//!
//! // ... do work ...
//!
//! // Blocks until SIGTERM or Ctrl+C
//! tasks.cancel_token().cancelled().await;
//! tasks.shutdown().await;
//! # });
//! ```

use crate::TaskPool;
use tracing::info;

/// Spawn a signal handler that cancels the pool on SIGTERM or Ctrl+C.
///
/// The handler task is spawned on the provided `TaskPool` and listens for:
/// - SIGTERM (Unix only)
/// - Ctrl+C (all platforms)
///
/// When either signal is received, the pool's cancel token is triggered,
/// which cascades to all tasks (including child tokens).
pub fn listen_for_cancel(tasks: &TaskPool) {
    let cancel_token = tasks.cancel_token().clone();
    crate::spawn!(tasks, "signal_handler", async move {
        wait_for_signal(&cancel_token).await;
        cancel_token.cancel();
    });
}

async fn wait_for_signal(cancel_token: &crate::CancellationToken) {
    #[cfg(unix)]
    {
        let mut term_handler =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                .expect("Failed to register signal handlers");
        tokio::select! {
            _ = term_handler.recv() => {
                info!("Received terminate signal, stopping...");
            }
            _ = tokio::signal::ctrl_c() => {
                info!("Received CTRL+C, stopping...");
            }
            _ = cancel_token.cancelled() => {}
        }
    }
    #[cfg(not(unix))]
    {
        tokio::select! {
            result = tokio::signal::ctrl_c() => {
                result.expect("Failed to listen for CTRL+C");
                info!("Received CTRL+C, stopping...");
            }
            _ = cancel_token.cancelled() => {}
        }
    }
}
