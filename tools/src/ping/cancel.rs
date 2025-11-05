pub fn listen_for_cancel(cancel_token: &tokio_util::sync::CancellationToken) {
    #[cfg(unix)]
    let mut term_handler =
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("Failed to register signal handlers");
    #[cfg(not(unix))]
    let mut term_handler = std::future::pending();

    let cancel_token = cancel_token.clone();
    tokio::spawn(async move {
        tokio::select! {
            _ = term_handler.recv() => {
                // Signal stop
                eprintln!("Received terminate signal, stopping...");
            }
            _ = tokio::signal::ctrl_c() => {
                // Signal stop
                eprintln!("Received CTRL+C, stopping...");
            }
            _ = cancel_token.cancelled() => {}
        }

        // Cancel everything
        cancel_token.cancel();
    });
}
