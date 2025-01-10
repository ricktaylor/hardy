use super::*;

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

        cancel_token.cancel();
    });
}

pub fn new_cancellable_set() -> (
    tokio::task::JoinSet<()>,
    tokio_util::sync::CancellationToken,
) {
    let cancel_token = tokio_util::sync::CancellationToken::new();
    let mut task_set = tokio::task::JoinSet::new();
    listen_for_cancel(&mut task_set, cancel_token.clone());
    (task_set, cancel_token)
}
