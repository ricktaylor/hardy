/// Spawns a task with optional tracing instrumentation.
///
/// This macro provides a convenient way to spawn tasks with tracing support.
/// When the `tracing` feature is enabled, it automatically adds span instrumentation.
///
/// # Syntax
///
/// ```text
/// // Simple case (no fields):
/// hardy_async::spawn!(pool, "task_name", async { ... })
///
/// // Complex case (with span fields - use parentheses):
/// hardy_async::spawn!(pool, "task_name", (?field1, field2 = value), async { ... })
/// ```
///
#[macro_export]
macro_rules! spawn {
    // Simple case: just task name and future (no fields)
    ($pool:expr, $name:literal, async $($rest:tt)*) => {{
        #[cfg(feature = "tracing")]
        {
            let task = async $($rest)*;
            let span = tracing::trace_span!(parent: None, $name);
            span.follows_from(tracing::Span::current());
            $pool.spawn(tracing::Instrument::instrument(task, span))
        }
        #[cfg(not(feature = "tracing"))]
        {
            $pool.spawn(async $($rest)*)
        }
    }};

    // Complex case: has fields before async
    // Fields are wrapped in parentheses for clear delimitation
    ($pool:expr, $name:literal, ($($field:tt)*), async $($rest:tt)*) => {{
        #[cfg(feature = "tracing")]
        {
            let task = async $($rest)*;
            // Pass fields directly to trace_span (handles any tracing field syntax)
            let span = tracing::trace_span!(parent: None, $name, $($field)*);
            span.follows_from(tracing::Span::current());
            $pool.spawn(tracing::Instrument::instrument(task, span))
        }
        #[cfg(not(feature = "tracing"))]
        {
            $pool.spawn(async $($rest)*)
        }
    }};
}
