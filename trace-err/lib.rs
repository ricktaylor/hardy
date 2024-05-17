// Shamelessly derived from the log_err crate

pub trait TraceErrResult<T, E: std::fmt::Debug + std::fmt::Display> {
    #[track_caller]
    fn trace_unwrap(self) -> T;

    #[track_caller]
    fn trace_expect(self, msg: &str) -> T;
}

impl<T, E: std::fmt::Debug + std::fmt::Display> TraceErrResult<T, E> for std::result::Result<T, E> {
    /// `unwrap`s the `Result`, and outputs error message (in exact same style as `unwrap`) through `error!` as well.
    fn trace_unwrap(self) -> T {
        match self {
            Ok(v) => v,
            Err(ref e) => {
                tracing::error!(target:"unwrap","{}: called `Result::unwrap()` on an `Err` value: {e}",std::panic::Location::caller());
                self.unwrap()
            }
        }
    }

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
    fn trace_unwrap(self) -> T;
    fn trace_expect(self, msg: &str) -> T;
}

impl<T> TraceErrOption<T> for std::option::Option<T> {
    /// `unwrap`s the `Option`, and outputs error message (in exact same style as `unwrap`) through `error!` as well.
    fn trace_unwrap(self) -> T {
        match self {
            Some(n) => n,
            None => {
                tracing::error!(target:"unwrap","called `Option::unwrap()` on a `None` value");
                self.unwrap()
            }
        }
    }

    /// `expect`s the `Option`, and outputs error message (in exact same style as `expect`) through `error!` as well.
    fn trace_expect(self, msg: &str) -> T {
        match self {
            Some(n) => n,
            None => {
                tracing::error!(target:"expect","{msg}");
                self.expect(msg)
            }
        }
    }
}
