/// Spawns a task with optional span instrumentation.
///
/// When the calling crate enables a feature that activates `tracing/attributes`,
/// the task is wrapped in a `trace`-level span linked to the spawning context
/// via `follows_from`. Without that feature, the task is spawned as-is.
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
        #[cfg(feature = "instrument")]
        {
            let task = async $($rest)*;
            let span = tracing::trace_span!(parent: None, $name);
            span.follows_from(tracing::Span::current());
            $pool.spawn(tracing::Instrument::instrument(task, span))
        }
        #[cfg(not(feature = "instrument"))]
        {
            $pool.spawn(async $($rest)*)
        }
    }};

    // Complex case: has fields before async
    // Fields are wrapped in parentheses for clear delimitation
    ($pool:expr, $name:literal, ($($field:tt)*), async $($rest:tt)*) => {{
        #[cfg(feature = "instrument")]
        {
            let task = async $($rest)*;
            // Pass fields directly to trace_span (handles any tracing field syntax)
            let span = tracing::trace_span!(parent: None, $name, $($field)*);
            span.follows_from(tracing::Span::current());
            $pool.spawn(tracing::Instrument::instrument(task, span))
        }
        #[cfg(not(feature = "instrument"))]
        {
            $pool.spawn(async $($rest)*)
        }
    }};
}
