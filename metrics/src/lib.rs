#![cfg_attr(not(feature = "metrics"), no_std)]

#[cfg(feature = "metrics")]
pub use metrics::*;

#[cfg(not(feature = "metrics"))]
mod noop {
    pub struct Counter;
    impl Counter {
        #[inline]
        pub fn increment(&self, _: u64) {}
        #[inline]
        pub fn absolute(&self, _: u64) {}
    }

    pub struct Gauge;
    impl Gauge {
        #[inline]
        pub fn set(&self, _: f64) {}
        #[inline]
        pub fn increment(&self, _: f64) {}
        #[inline]
        pub fn decrement(&self, _: f64) {}
    }

    pub enum Unit {
        Count,
        Bytes,
        Seconds, /* ... */
    }
}

#[cfg(not(feature = "metrics"))]
pub use noop::*;

#[cfg(not(feature = "metrics"))]
#[macro_export]
macro_rules! counter {
    ($name:expr $(, $key:expr => $value:expr)* $(,)?) => {{
        if false {
            let _ = &$name;
            $(
                let _ = &$key;
                let _ = &$value;
            )*
        }

        $crate::Counter
    }};
}

#[cfg(not(feature = "metrics"))]
#[macro_export]
macro_rules! gauge {
    ($name:expr $(, $key:expr => $value:expr)* $(,)?) => {{
        if false {
            let _ = &$name;
            $(
                let _ = &$key;
                let _ = &$value;
            )*
        }

        $crate::Gauge
    }};
}

#[cfg(not(feature = "metrics"))]
#[macro_export]
macro_rules! describe_counter {
    ($($_:tt)*) => {
        ()
    };
}

#[cfg(not(feature = "metrics"))]
#[macro_export]
macro_rules! describe_gauge {
    ($($_:tt)*) => {
        ()
    };
}
