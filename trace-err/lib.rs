/*!
# Trace_Err
A small extension to the [tracing](https://crates.io/crates/tracing) crate, which provides a single method for `core::result::Result<T, E>` and `core::option::Option<T>`.

Adds `trace_expect` to `Result`, which invoke the `tracing::error!` macro (in case of `Result::Err`) in _addition_ to unwrapping/expecting the `Result`.

Adds `trace_expect` to `Option``, which invoke the `tracing::error!` macro (in case of `Option::None`) in _addition_ to unwrapping/expecting the `Option`.

Shamelessly derived from the [Log_Err](https://crates.io/crates/log_err) crate.

Shorthand for:

```should_panic
# use tracing::error;
# fn something() -> Result<(), &'static str> {Err("there was some problem")}
# let msg = "Some message";
something().map_err(|e| tracing::error!("{}: {:?}", msg, e)).expect(msg)
```

Example:

```should_panic
# use std::fs::File;
# use trace_err::*;
let mut file = File::open("foo.txt").trace_expect("Error creating file");
```
```text
# Error will be logged with the error! macro
2024-06-12T09:31:23.933299Z ERROR expect: trace-err/lib.rs:87:39: Error creating file: Os { code: 2, kind: NotFound, message: "No such file or directory" }

# Main program panic'ing with same message
thread 'main' panicked at trace-err/lib.rs:87:39:
Error creating file: Os { code: 2, kind: NotFound, message: "No such file or directory" }
```
*/

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

#[cfg(test)]
mod test {
    use super::*;
    use tracing_subscriber;

    static INIT: std::sync::Once = std::sync::Once::new();

    fn setup() {
        tracing_subscriber::fmt::init();
    }

    #[test]
    #[should_panic(expected = "We caught: \"An Error!\"")]
    fn test_expect() {
        INIT.call_once(setup);
        Result::<(), &str>::Err("An Error!").trace_expect("We caught");
    }

    #[test]
    #[should_panic(expected = "It's None!")]
    fn test_unwrap() {
        INIT.call_once(setup);
        let _ = Option::<()>::None.trace_expect("It's None!");
    }
}
