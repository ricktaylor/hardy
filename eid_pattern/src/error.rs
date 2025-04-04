use std::ops::Range;
use thiserror::Error;

#[derive(Debug, Clone)]
pub struct Span(Range<usize>);

impl Span {
    pub fn new(start: usize, end: usize) -> Self {
        Self(start..end)
    }

    pub fn subset(&self, l: usize) -> Self {
        Self(self.0.start..self.0.start + l)
    }

    pub fn inc(&mut self, i: usize) {
        self.0.start += i;
        self.0.end = self.0.start;
    }

    pub fn offset(&mut self, i: usize) {
        self.0.start += i;
        self.0.end += i;
    }
}

impl std::fmt::Display for Span {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.0.start == self.0.end {
            write!(f, "{}", self.0.start)
        } else {
            write!(f, "{}..{}", self.0.start, self.0.end)
        }
    }
}

#[derive(Error, Debug)]
pub enum EidPatternError {
    #[error("Expecting '{0}' at {1}")]
    Expecting(String, Span),

    #[error("Invalid scheme at {0}")]
    InvalidScheme(Span),

    #[error("Invalid number or number range as {0}")]
    InvalidIpnNumber(Span),

    #[error("Expecting regular expression as {0}")]
    ExpectingRegEx(Span),

    #[error("{1} at {0}")]
    InvalidRegEx(#[source] regex::Error, Span),

    #[error("{0} at {1}")]
    InvalidUtf8(#[source] std::string::FromUtf8Error, Span),
}
