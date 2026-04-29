/*!
OpenTelemetry integration for Hardy binaries.

Provides one-call initialization of the full OpenTelemetry observability
stack — distributed traces, metrics, and structured logs — exported via
OTLP/gRPC. The [`init`] function wires up providers, installs a
`tracing-subscriber` pipeline, and returns an [`OtelGuard`] whose [`Drop`]
implementation flushes and shuts down every provider cleanly.
*/

use opentelemetry::{KeyValue, global, metrics::MeterProvider, trace::TracerProvider};
use opentelemetry_sdk::{
    Resource, logs::SdkLoggerProvider, metrics::SdkMeterProvider, trace::SdkTracerProvider,
};
use opentelemetry_semantic_conventions::{SCHEMA_URL, resource::SERVICE_VERSION};
use tracing_subscriber::{EnvFilter, Layer, layer::SubscriberExt, util::SubscriberInitExt};

mod metrics_otel;

fn endpoint_configured() -> bool {
    std::env::var_os("OTEL_EXPORTER_OTLP_ENDPOINT").is_some()
        || std::env::var_os("OTEL_EXPORTER_OTLP_TRACES_ENDPOINT").is_some()
        || std::env::var_os("OTEL_EXPORTER_OTLP_METRICS_ENDPOINT").is_some()
        || std::env::var_os("OTEL_EXPORTER_OTLP_LOGS_ENDPOINT").is_some()
}

fn init_tracer(resource: &Resource) -> SdkTracerProvider {
    let exporter = opentelemetry_otlp::SpanExporter::builder()
        .with_tonic()
        .build()
        .expect("Failed to create span exporter");

    let tracer_provider = SdkTracerProvider::builder()
        // Customize sampling strategy
        // .with_sampler(Sampler::ParentBased(Box::new(Sampler::TraceIdRatioBased(
        //     1.0,
        // ))))
        // If export trace to AWS X-Ray, you can use XrayIdGenerator
        // .with_id_generator(RandomIdGenerator::default())
        .with_resource(resource.clone())
        .with_batch_exporter(exporter)
        .build();

    // Set the global tracer provider using a clone of the tracer_provider.
    // Setting global tracer provider is required if other parts of the application
    // uses global::tracer() or global::tracer_with_version() to get a tracer.
    // Cloning simply creates a new reference to the same tracer provider. It is
    // important to hold on to the tracer_provider here, so as to invoke
    // shutdown on it when application ends.
    global::set_tracer_provider(tracer_provider.clone());

    tracer_provider
}

fn init_metrics(resource: &Resource, pkg_name: &'static str) -> SdkMeterProvider {
    let exporter = opentelemetry_otlp::MetricExporter::builder()
        .with_tonic()
        //.with_temporality(opentelemetry_sdk::metrics::Temporality::default())
        .build()
        .expect("Failed to create metric exporter");

    let meter_provider = SdkMeterProvider::builder()
        .with_periodic_exporter(exporter)
        .with_resource(resource.clone())
        .build();

    let meter = meter_provider.meter(pkg_name);
    let recorder = metrics_otel::OpenTelemetryRecorder::new(meter);
    metrics::set_global_recorder(recorder).expect("failed to install recorder");

    // Set the global meter provider using a clone of the meter_provider.
    // Setting global meter provider is required if other parts of the application
    // uses global::meter() or global::meter_with_version() to get a meter.
    // Cloning simply creates a new reference to the same meter provider. It is
    // important to hold on to the meter_provider here, so as to invoke
    // shutdown on it when application ends.
    //global::set_meter_provider(meter_provider.clone());

    meter_provider
}

fn init_logs(resource: &Resource) -> SdkLoggerProvider {
    let exporter = opentelemetry_otlp::LogExporter::builder()
        .with_tonic()
        .build()
        .expect("Failed to create log exporter");

    SdkLoggerProvider::builder()
        .with_resource(resource.clone())
        .with_batch_exporter(exporter)
        .build()
}

/// Initialize OpenTelemetry tracing, metrics, and logging providers.
///
/// Sets up OTLP/gRPC exporters for traces, metrics, and logs, installs a
/// `tracing-subscriber` with both a formatted console layer and an
/// OpenTelemetry bridge layer, and registers a `metrics` recorder backed by
/// OpenTelemetry. The returned [`OtelGuard`] must be held for the lifetime
/// of the application; dropping it flushes and shuts down all providers.
///
/// The log level defaults to `level` but can be overridden via the
/// `RUST_LOG` environment variable.
pub fn init(pkg_name: &'static str, pkg_ver: &'static str, level: tracing::Level) -> OtelGuard {
    // Create a filter using RUST_LOG if set, falling back to the configured level
    let make_filter = || {
        EnvFilter::builder()
            .with_default_directive(
                tracing_subscriber::filter::LevelFilter::from_level(level).into(),
            )
            .from_env_lossy()
    };

    let resource = Resource::builder()
        .with_service_name(pkg_name)
        .with_schema_url([KeyValue::new(SERVICE_VERSION, pkg_ver)], SCHEMA_URL)
        .build();

    let tracer_provider = init_tracer(&resource);
    let meter_provider = init_metrics(&resource, pkg_name);
    let logger_provider = init_logs(&resource);

    // Create a new OpenTelemetryTracingBridge using the above LoggerProvider.
    let otel_layer =
        opentelemetry_appender_tracing::layer::OpenTelemetryTracingBridge::new(&logger_provider);

    // To prevent a telemetry-induced-telemetry loop, OpenTelemetry's own internal
    // logging is properly suppressed. However, logs emitted by external components
    // (such as reqwest, tonic, etc.) are not suppressed as they do not propagate
    // OpenTelemetry context. Until this issue is addressed
    // (https://github.com/open-telemetry/opentelemetry-rust/issues/2877),
    // filtering like this is the best way to suppress such logs.
    //
    // Note: This filtering will also drop logs from these components even when
    // they are used outside of the OTLP Exporter.
    const SUPPRESSED_CRATES: &[&str] = &[
        "reqwest",
        "tonic",
        "tower",
        "h2",
        "hyper_util",
        "opentelemetry_otlp",
        "opentelemetry_sdk",
    ];

    let add_common_filters = |filter: EnvFilter| -> EnvFilter {
        SUPPRESSED_CRATES.iter().fold(filter, |f, crate_name| {
            f.add_directive(format!("{crate_name}=off").parse().unwrap())
        })
    };

    let filter_otel = add_common_filters(make_filter());
    let otel_layer = otel_layer.with_filter(filter_otel);

    let filter_fmt =
        add_common_filters(make_filter()).add_directive("opentelemetry=off".parse().unwrap());
    let fmt_layer = tracing_subscriber::fmt::layer().with_filter(filter_fmt);

    let tracer = tracer_provider.tracer("tracing-otel-subscriber");

    // Initialize the tracing subscriber with the OpenTelemetry layer and the
    // Fmt layer.
    tracing_subscriber::registry()
        .with(otel_layer)
        .with(fmt_layer)
        // .with(tracing_opentelemetry::MetricsLayer::new(
        //     meter_provider.clone(),
        // ))
        .with(tracing_opentelemetry::OpenTelemetryLayer::new(tracer))
        .init();

    OtelGuard {
        tracer_provider,
        meter_provider,
        logger_provider,
        endpoint_configured: endpoint_configured(),
    }
}

/// RAII guard that owns the OpenTelemetry trace, metric, and log providers.
///
/// Dropping this guard flushes all pending telemetry and shuts down each
/// provider. Hold it in `main` until the application exits.
#[must_use = "dropping the OtelGuard immediately shuts down all telemetry exporters"]
pub struct OtelGuard {
    tracer_provider: SdkTracerProvider,
    meter_provider: SdkMeterProvider,
    logger_provider: SdkLoggerProvider,
    endpoint_configured: bool,
}

impl OtelGuard {
    /// Force-flush all pending telemetry to the collector.
    /// Call before shutdown for short-lived processes where the periodic
    /// export interval (60s for metrics) may not have fired.
    pub fn force_flush(&self) {
        self.tracer_provider.force_flush().unwrap_or_else(|e| {
            if self.endpoint_configured {
                tracing::warn!("OTEL tracer flush failed: {e}");
            }
        });
        self.meter_provider.force_flush().unwrap_or_else(|e| {
            if self.endpoint_configured {
                tracing::warn!("OTEL meter flush failed: {e}");
            }
        });
        self.logger_provider.force_flush().unwrap_or_else(|e| {
            if self.endpoint_configured {
                eprintln!("Warning: OTEL logger flush failed: {e}");
            }
        });
    }
}

impl Drop for OtelGuard {
    fn drop(&mut self) {
        // Flush pending telemetry before shutdown to avoid timeout errors
        // when the periodic export interval hasn't fired yet.
        self.force_flush();

        self.tracer_provider.shutdown().unwrap_or_else(|e| {
            if self.endpoint_configured {
                tracing::warn!("OTEL tracer provider did not shut down cleanly: {e}");
            }
        });
        self.meter_provider.shutdown().unwrap_or_else(|e| {
            if self.endpoint_configured {
                tracing::warn!("OTEL meter provider did not shut down cleanly: {e}");
            }
        });
        self.logger_provider.shutdown().unwrap_or_else(|e| {
            if self.endpoint_configured {
                eprintln!("Warning: OTEL logger provider did not shut down cleanly: {e}");
            }
        });
    }
}
