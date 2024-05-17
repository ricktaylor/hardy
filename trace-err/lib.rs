// Shamelessly derived from the log_err crate

pub trait TraceErrResult<T, E: std::fmt::Debug> {
    fn trace_unwrap(self) -> T;
    fn trace_expect(self, msg: &str) -> T;
}

impl<T, E: std::fmt::Debug> TraceErrResult<T, E> for std::result::Result<T, E> {
    /// `unwrap`s the `Result`, and outputs error message (in exact same style as `unwrap`) through `error!` as well.
    fn trace_unwrap(self) -> T {
        self.inspect_err(|e| tracing::error!("called `Result::unwrap()` on an `Err` value: {e:?}"))
            .unwrap()
    }

    /// `expect`s the `Result`, and outputs error message (in exact same style as `expect`) through `error!` as well.
    fn trace_expect(self, msg: &str) -> T {
        self.inspect_err(|e| tracing::error!("{msg}: {e:?}"))
            .expect(msg)
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
                tracing::error!("called `Option::unwrap()` on a `None` value");
                self.unwrap()
            }
        }
    }

    /// `expect`s the `Option`, and outputs error message (in exact same style as `expect`) through `error!` as well.
    fn trace_expect(self, msg: &str) -> T {
        match self {
            Some(n) => n,
            None => {
                tracing::error!("{msg}");
                self.expect(msg)
            }
        }
    }
}
