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
