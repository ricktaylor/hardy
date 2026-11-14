//! `LogLevel`: the `log-level` setting's parsing, defaults and spelling.

use hardy_otel::{LogLevel, ParseLogLevelError};
use tracing::Level;

const LEVELS: [(Level, &str); 5] = [
    (Level::ERROR, "error"),
    (Level::WARN, "warn"),
    (Level::INFO, "info"),
    (Level::DEBUG, "debug"),
    (Level::TRACE, "trace"),
];

#[test]
fn default_is_info() {
    assert_eq!(LogLevel::default().get(), Level::INFO);
}

#[test]
fn names_parse_in_any_case() {
    for (level, name) in LEVELS {
        assert_eq!(name.parse::<LogLevel>().unwrap().get(), level);
        assert_eq!(
            name.to_uppercase().parse::<LogLevel>().unwrap().get(),
            level
        );
    }
}

#[test]
fn numeric_forms_parse() {
    for (number, (level, _)) in (1..).zip(LEVELS) {
        assert_eq!(number.to_string().parse::<LogLevel>().unwrap().get(), level);
    }
}

#[test]
fn unknown_names_are_refused() {
    for input in ["banana", "", "0", "6", "warning"] {
        assert_eq!(
            input.parse::<LogLevel>(),
            Err(ParseLogLevelError::new(input))
        );
    }
}

#[test]
fn display_writes_the_lowercase_name() {
    for (level, name) in LEVELS {
        assert_eq!(LogLevel::new(level).to_string(), name);
    }
}

#[cfg(feature = "serde")]
mod serde {
    use serde_json::{Value, from_value, to_value};

    use super::*;

    #[test]
    fn deserializes_from_a_name() {
        let level: LogLevel = from_value(Value::from("debug")).unwrap();
        assert_eq!(level.get(), Level::DEBUG);
    }

    #[test]
    fn serializes_the_lowercase_name_and_parses_back() {
        for (level, name) in LEVELS {
            let value = to_value(LogLevel::new(level)).unwrap();
            assert_eq!(value, Value::from(name));
            assert_eq!(from_value::<LogLevel>(value).unwrap().get(), level);
        }
    }

    // The error carries the parse error's message, naming the input.
    #[test]
    fn an_unknown_name_is_refused() {
        let err = from_value::<LogLevel>(Value::from("banana")).unwrap_err();
        assert_eq!(
            err.to_string(),
            ParseLogLevelError::new("banana").to_string()
        );
    }
}
