// Shamelessly derived from the log_err crate

pub trait TraceErrResult<T, E: core::fmt::Debug> {
    #[track_caller]
    fn trace_expect(self, msg: &str) -> T;
}

impl<T, E: core::fmt::Debug> TraceErrResult<T, E> for core::result::Result<T, E> {
    /// `expect`s the `Result`, and outputs error message (in exact same style as `expect`) through `error!` as well.
    fn trace_expect(self, msg: &str) -> T {
        match self {
            Ok(v) => v,
            Err(ref e) => {
                tracing::error!(target: "expect","{}: {msg}: {e:?}",core::panic::Location::caller());
                self.expect(msg)
            }
        }
    }
}

pub trait TraceErrOption<T> {
    #[track_caller]
    fn trace_expect(self, msg: &str) -> T;
}

impl<T> TraceErrOption<T> for core::option::Option<T> {
    /// `expect`s the `Option`, and outputs error message (in exact same style as `expect`) through `error!` as well.
    fn trace_expect(self, msg: &str) -> T {
        match self {
            Some(n) => n,
            None => {
                tracing::error!(target:"expect","{}: {msg}",core::panic::Location::caller());
                self.expect(msg)
            }
        }
    }
}
