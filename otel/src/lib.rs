/*!
Logging and OpenTelemetry integration for Hardy binaries.

[`LogLevel`] is the `log-level` setting every Hardy binary reads from its
configuration, and [`init`] installs the binary's `tracing` subscriber at
that level, returning an [`OtelGuard`] to hold until exit. Without the
`otlp` feature, `init` installs console logging only and the guard holds
nothing. With it, `init` also installs the full OpenTelemetry
observability stack — distributed traces, metrics, and structured logs
exported via OTLP/gRPC — and the guard's [`Drop`] flushes and shuts down
every provider. A binary calls `init` the same way in both builds.

Either way, the `RUST_LOG` environment variable overrides the configured
level.

# Key types

- [`LogLevel`] — the default log level, `info` unless configured.
- [`init`] / [`OtelGuard`] — the subscriber, and the guard to hold.

# Feature flags

- `serde` — adds `Serialize`/`Deserialize` impls to [`LogLevel`].
- `otlp` — adds the OTLP/gRPC exporters for traces, metrics, and logs,
  and the `metrics` facade recorder, to what [`init`] installs.
*/

#[cfg(not(feature = "otlp"))]
mod console;
#[cfg(feature = "otlp")]
mod metrics_otel;
#[cfg(feature = "otlp")]
mod otlp;

use core::{
    fmt::{self, Display, Formatter},
    str::FromStr,
};

#[cfg(feature = "serde")]
use serde::{Deserialize, Deserializer, Serialize, Serializer, de::Error as _};
use thiserror::Error;
use tracing::Level;
use tracing_subscriber::{EnvFilter, filter::LevelFilter};

#[cfg(not(feature = "otlp"))]
pub use self::console::{OtelGuard, init};
#[cfg(feature = "otlp")]
pub use self::otlp::{OtelGuard, init};

/// The default log level of a Hardy binary: [`Level::INFO`] unless
/// configured.
///
/// Parses from `error`, `warn`, `info`, `debug`, or `trace` (in any case)
/// or their numeric forms `1` to `5`; with the `serde` feature it
/// (de)serializes the same way, writing the lowercase name. The
/// `RUST_LOG` environment variable, when set, overrides it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct LogLevel(Level);

impl LogLevel {
    /// Wraps a [`Level`].
    pub const fn new(level: Level) -> Self {
        Self(level)
    }

    /// The wrapped [`Level`].
    pub const fn get(self) -> Level {
        self.0
    }

    // The filter every subscriber layer starts from: `RUST_LOG` when set,
    // otherwise this level.
    pub(crate) fn env_filter(self) -> EnvFilter {
        EnvFilter::builder()
            .with_default_directive(LevelFilter::from_level(self.0).into())
            .from_env_lossy()
    }
}

impl Default for LogLevel {
    fn default() -> Self {
        Self(Level::INFO)
    }
}

impl From<Level> for LogLevel {
    fn from(level: Level) -> Self {
        Self(level)
    }
}

impl From<LogLevel> for Level {
    fn from(level: LogLevel) -> Self {
        level.0
    }
}

impl Display for LogLevel {
    /// Writes the lowercase level name, the spelling the config files use.
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_str(match self.0 {
            Level::ERROR => "error",
            Level::WARN => "warn",
            Level::INFO => "info",
            Level::DEBUG => "debug",
            Level::TRACE => "trace",
        })
    }
}

/// Returned when a [`LogLevel`] fails to parse.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("invalid log level '{0}': expected error, warn, info, debug, or trace")]
pub struct ParseLogLevelError(String);

impl ParseLogLevelError {
    /// The error for `input`, the text that failed to parse.
    pub fn new(input: impl Into<String>) -> Self {
        Self(input.into())
    }

    /// The text that failed to parse.
    pub fn input(&self) -> &str {
        &self.0
    }
}

impl FromStr for LogLevel {
    type Err = ParseLogLevelError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        s.parse().map(Self).map_err(|_| ParseLogLevelError::new(s))
    }
}

#[cfg(feature = "serde")]
impl Serialize for LogLevel {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

#[cfg(feature = "serde")]
impl<'de> Deserialize<'de> for LogLevel {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer)?
            .parse()
            .map_err(D::Error::custom)
    }
}
