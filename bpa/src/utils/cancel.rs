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
            _ = term_handler.recv() =>
                {
                    // Signal stop
                    log::info!("Received terminate signal, stopping...");
                    cancel_token.cancel();
                }
            _ = tokio::signal::ctrl_c() =>
                {
                    // Signal stop
                    log::info!("Received CTRL+C, stopping...");
                    cancel_token.cancel();
                }
            _ = cancel_token.cancelled() => {}
        }
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

pub async fn cancellable_sleep(
    duration: time::Duration,
    cancel_token: &tokio_util::sync::CancellationToken,
) -> bool {
    if !duration.is_positive() {
        return true;
    }

    let timer = tokio::time::sleep(tokio::time::Duration::new(
        duration.whole_seconds() as u64,
        duration.subsec_nanoseconds() as u32,
    ));
    tokio::pin!(timer);

    tokio::select! {
        () = &mut timer => true,
        _ = cancel_token.cancelled() => false
    }
}
