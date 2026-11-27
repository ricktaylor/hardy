// `init` without the `otlp` feature: console logging only, and a guard
// with nothing to flush.

use tracing_subscriber::{Layer, fmt, layer::SubscriberExt, util::SubscriberInitExt};

use crate::LogLevel;

/// Installs a console `tracing` subscriber that logs at `level`, unless
/// `RUST_LOG` is set, in which case `RUST_LOG` decides.
///
/// Built without the `otlp` feature, nothing is exported; `pkg_name` and
/// `pkg_ver` name the service only to the exporters. The returned
/// [`OtelGuard`] holds nothing, but keeping it alive matches the `otlp`
/// build, so a binary calls `init` the same way in both.
///
/// # Panics
///
/// If a global `tracing` subscriber is already installed.
pub fn init(_pkg_name: &'static str, _pkg_ver: &'static str, level: LogLevel) -> OtelGuard {
    tracing_subscriber::registry()
        .with(fmt::layer().with_filter(level.env_filter()))
        .init();
    OtelGuard(())
}

/// The guard [`init`] returns. Built without the `otlp` feature there are
/// no exporters, so it holds nothing and dropping it does nothing.
#[must_use = "hold the guard until exit: with the `otlp` feature, dropping it shuts down telemetry export"]
#[derive(Debug)]
pub struct OtelGuard(());

impl OtelGuard {
    /// Flushes pending telemetry: nothing to do without the `otlp` feature.
    pub fn force_flush(&self) {}
}
