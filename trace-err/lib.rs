// Shamelessly derived from the log_err crate

pub trait TraceErrResult<T, E: std::fmt::Debug + std::fmt::Display> {
    #[track_caller]
    fn trace_expect(self, msg: &str) -> T;
}

impl<T, E: std::fmt::Debug + std::fmt::Display> TraceErrResult<T, E> for std::result::Result<T, E> {
    /// `expect`s the `Result`, and outputs error message (in exact same style as `expect`) through `error!` as well.
    fn trace_expect(self, msg: &str) -> T {
        match self {
            Ok(v) => v,
            Err(ref e) => {
                tracing::error!(target: "expect","{}: {msg}: {e}",std::panic::Location::caller());
                self.expect(msg)
            }
        }
    }
}

pub trait TraceErrOption<T> {
    #[track_caller]
    fn trace_expect(self, msg: &str) -> T;
}

impl<T> TraceErrOption<T> for std::option::Option<T> {
    /// `expect`s the `Option`, and outputs error message (in exact same style as `expect`) through `error!` as well.
    fn trace_expect(self, msg: &str) -> T {
        match self {
            Some(n) => n,
            None => {
                tracing::error!(target:"expect","{}: {msg}",std::panic::Location::caller());
                self.expect(msg)
            }
        }
    }
}
