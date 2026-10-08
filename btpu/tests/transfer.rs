//! The public `transfer` API: window sizes and the window-full error.

mod common;

use hardy_btpu::{
    OutOfRange, ParseError,
    transfer::{Error, WindowSize},
};

use self::common::window_size as ws;

#[test]
fn window_size_boundaries() {
    // Section 5: 4..=4095 (less than 2^12).
    assert_eq!(WindowSize::MIN.get(), 4);
    assert_eq!(WindowSize::MAX.get(), 4095);
    assert_eq!(WindowSize::try_from(4), Ok(WindowSize::MIN));
    assert_eq!(WindowSize::try_from(4095), Ok(WindowSize::MAX));
    for value in [0, 3, 4096] {
        assert_eq!(WindowSize::new(value), None);
        assert_eq!(
            WindowSize::try_from(value),
            Err(OutOfRange {
                name: "window size",
                value: u64::from(value),
                min: 4,
                max: Some(4095),
            })
        );
    }
}

#[test]
fn window_size_default_is_recommended() {
    assert_eq!(WindowSize::default(), WindowSize::DEFAULT);
    assert_eq!(WindowSize::DEFAULT.get(), 16);
    assert_eq!(WindowSize::DEFAULT.to_string(), "16");
}

#[test]
fn window_size_parses_and_formats_as_its_integer() {
    assert_eq!("16".parse(), Ok(WindowSize::DEFAULT));
    assert_eq!(
        "4096".parse::<WindowSize>(),
        Err(ParseError::OutOfRange(
            WindowSize::try_from(4096).unwrap_err()
        ))
    );
    // Too wide for the u16 it parses into: a syntax error, not a range one.
    assert_eq!(
        "65536".parse::<WindowSize>(),
        Err(ParseError::Syntax {
            name: "window size",
            source: "65536".parse::<u16>().unwrap_err(),
        })
    );
    let w = WindowSize::MAX;
    assert_eq!(
        format!("{w:b} {w:o} {w:x} {w:X}"),
        "111111111111 7777 fff FFF"
    );
}

#[test]
fn window_full_names_the_window_size() {
    assert_eq!(
        Error::WindowFull { window_size: ws(4) }.to_string(),
        "Transfer window full (size 4)"
    );
}
