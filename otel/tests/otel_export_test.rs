/*
Test harness for OTEL export verification.

Initialises hardy_otel, emits traces, metrics, and logs,
then shuts down cleanly to flush to the collector.

Run via: cargo test -p hardy-otel --test otel_export_test -- --ignored
(or via otel/tests/test_otel_export.sh which starts a collector)
*/

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore] // Requires an OTLP collector — run via test_otel_export.sh
async fn export_telemetry() {
    let guard = hardy_otel::init("otel-test", "0.0.0", tracing::Level::INFO);

    // Emit traces via tracing
    tracing::info!("test log message");
    tracing::warn!(test_field = "value", "structured log");

    {
        let _span = tracing::info_span!("test_operation").entered();
        tracing::info!("inside span");

        {
            let _child = tracing::info_span!("child_operation").entered();
            tracing::debug!("nested span work");
        }
    }

    // Emit metrics via metrics crate
    metrics::counter!("test_bundles_processed").increment(5);
    metrics::gauge!("test_bundles_pending").set(3.0);
    metrics::histogram!("test_processing_latency").record(0.042);

    // Yield to let tonic establish gRPC connections for all three providers.
    // multi_thread runtime is required — tonic needs background workers for HTTP/2.
    tokio::time::sleep(std::time::Duration::from_secs(1)).await;

    // Explicit flush while the tokio runtime is fully active. OtelGuard::drop()
    // also calls force_flush(), which works correctly in production (server binaries
    // drop the guard during main() cleanup while the runtime is alive). But in
    // #[tokio::test], the runtime tears down its worker threads immediately after
    // the async fn returns, before drop() runs — so the tonic gRPC flush for
    // metrics and logs fails silently. This explicit call ensures all three
    // providers flush while workers are still available.
    guard.force_flush();

    // Drop guard — shutdown() called automatically
    drop(guard);
}
