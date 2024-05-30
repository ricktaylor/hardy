use super::*;
use std::ops::Range;
use thiserror::Error;

mod dtn_pattern;
mod ipn_pattern;

pub use dtn_pattern::*;
pub use ipn_pattern::*;

#[derive(Default, Debug, Clone)]
pub struct Span(Range<usize>);

impl Span {
    fn new(start: usize, end: usize) -> Self {
        Self(Range { start, end })
    }

    fn subset(&self, l: usize) -> Self {
        Self(Range {
            start: self.0.start,
            end: self.0.start + l,
        })
    }

    fn inc(&mut self, i: usize) {
        self.0.start += i;
        self.0.end = self.0.start;
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
pub enum Error {
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

#[derive(Debug)]
pub enum EidPattern {
    Set(Vec<EidPatternItem>),
    Any,
}

impl EidPattern {
    pub fn is_match(&self, eid: &Eid) -> bool {
        match self {
            EidPattern::Any => true,
            EidPattern::Set(items) => items.iter().any(|i| i.is_match(eid)),
        }
    }

    pub fn is_exact(&self) -> Option<Eid> {
        match self {
            EidPattern::Any => None,
            EidPattern::Set(items) => {
                if items.len() != 1 {
                    None
                } else {
                    items[0].is_exact()
                }
            }
        }
    }
}

/*
eid-pattern = any-scheme-item / eid-pattern-set
any-scheme-item = wildcard ":" multi-wildcard
eid-pattern-set = eid-pattern-item *( "|" eid-pattern-item )
*/
impl std::str::FromStr for EidPattern {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s == "*:**" {
            Ok(EidPattern::Any)
        } else {
            let mut v = Vec::new();
            let mut span = Span::default();
            for s in s.split('|') {
                v.push(EidPatternItem::parse(s, &mut span)?);
            }
            Ok(EidPattern::Set(v))
        }
    }
}

impl From<Eid> for EidPattern {
    fn from(value: Eid) -> Self {
        match value {
            Eid::Null => EidPattern::Set(vec![
                EidPatternItem::DtnPatternItem(DtnPatternItem::None),
                EidPatternItem::IpnPatternItem(IpnPatternItem {
                    allocator_id: IpnPattern::Range(vec![IpnInterval::Number(0)]),
                    node_number: IpnPattern::Range(vec![IpnInterval::Number(0)]),
                    service_number: IpnPattern::Range(vec![IpnInterval::Number(0)]),
                }),
            ]),
            Eid::LocalNode { service_number } => {
                EidPattern::Set(vec![EidPatternItem::IpnPatternItem(IpnPatternItem {
                    allocator_id: IpnPattern::Range(vec![IpnInterval::Number(0)]),
                    node_number: IpnPattern::Range(vec![IpnInterval::Number(u32::MAX)]),
                    service_number: IpnPattern::Range(vec![IpnInterval::Number(service_number)]),
                })])
            }
            Eid::Ipn2 {
                allocator_id,
                node_number,
                service_number,
            }
            | Eid::Ipn3 {
                allocator_id,
                node_number,
                service_number,
            } => EidPattern::Set(vec![EidPatternItem::IpnPatternItem(IpnPatternItem {
                allocator_id: IpnPattern::Range(vec![IpnInterval::Number(allocator_id)]),
                node_number: IpnPattern::Range(vec![IpnInterval::Number(node_number)]),
                service_number: IpnPattern::Range(vec![IpnInterval::Number(service_number)]),
            })]),
            Eid::Dtn {
                node_name,
                mut demux,
            } => EidPattern::Set(vec![EidPatternItem::DtnPatternItem(
                DtnPatternItem::DtnSsp(DtnSsp {
                    authority: DtnAuthPattern::PatternMatch(PatternMatch::Exact(node_name)),
                    last: demux
                        .pop()
                        .map(|s| {
                            DtnLastPattern::Single(DtnSinglePattern::PatternMatch(
                                PatternMatch::Exact(s),
                            ))
                        })
                        .unwrap_or(DtnLastPattern::Single(DtnSinglePattern::PatternMatch(
                            PatternMatch::Exact("".to_string()),
                        ))),
                    singles: demux
                        .into_iter()
                        .map(|s| DtnSinglePattern::PatternMatch(PatternMatch::Exact(s)))
                        .collect(),
                }),
            )]),
        }
    }
}

#[derive(Debug)]
pub enum EidPatternItem {
    IpnPatternItem(IpnPatternItem),
    DtnPatternItem(DtnPatternItem),
    AnyNumericScheme(u64),
    AnyTextScheme(String),
}

impl EidPatternItem {
    fn is_match(&self, eid: &Eid) -> bool {
        match self {
            EidPatternItem::IpnPatternItem(i) => i.is_match(eid),
            EidPatternItem::DtnPatternItem(i) => i.is_match(eid),
            _ => false,
        }
    }

    fn is_exact(&self) -> Option<Eid> {
        match self {
            EidPatternItem::IpnPatternItem(i) => i.is_exact(),
            EidPatternItem::DtnPatternItem(i) => i.is_exact(),
            _ => None,
        }
    }

    /*
    eid-pattern-item = scheme-pat-item / any-ssp-item
    scheme-pat-item = ipn-pat-item / dtn-pat-item
    any-ssp-item = (scheme / non-zero-number) ":" multi-wildcard
    scheme = ALPHA *( ALPHA / DIGIT / "+" / "-" / "." )
    non-zero-number = (%x31-39 *DIGIT)
    */
    fn parse(s: &str, span: &mut Span) -> Result<Self, Error> {
        let Some((s1, s2)) = s.split_once(':') else {
            return Err(Error::Expecting(
                ":".to_string(),
                span.subset(s.chars().count()),
            ));
        };
        match s1 {
            "ipn" => {
                // ipn-pat-item = "ipn:" ipn-ssp
                span.inc(4);
                Ok(EidPatternItem::IpnPatternItem(IpnPatternItem::parse(
                    s2, span,
                )?))
            }
            "dtn" => {
                // dtn-pat-item = "dtn:" dtn-ssp
                span.inc(4);
                Ok(EidPatternItem::DtnPatternItem(DtnPatternItem::parse(
                    s2, span,
                )?))
            }
            _ => match s1.chars().nth(0) {
                Some('1'..='9') => {
                    let Ok(v) = s1.parse::<u64>() else {
                        return Err(Error::InvalidScheme(span.subset(s1.chars().count())));
                    };

                    if v == 0 {
                        return Err(Error::InvalidScheme(span.subset(s1.chars().count())));
                    }

                    span.inc(s1.chars().count() + 1);
                    if s2 != "**" {
                        return Err(Error::Expecting(
                            "**".to_string(),
                            span.subset(s2.chars().count()),
                        ));
                    }
                    span.inc(2);
                    match v {
                        1 => Ok(EidPatternItem::IpnPatternItem(IpnPatternItem::new_any())),
                        2 => Ok(EidPatternItem::DtnPatternItem(DtnPatternItem::new_any())),
                        _ => Ok(EidPatternItem::AnyNumericScheme(v)),
                    }
                }
                Some('A'..='Z') | Some('a'..='z') => {
                    for c in s1.chars() {
                        if !matches!(c,'A'..='Z' | 'a'..='z' | '0'..='9' | '+' | '-' | '.') {
                            return Err(Error::InvalidScheme(span.subset(s1.chars().count())));
                        }
                        span.inc(1);
                    }

                    span.inc(1);
                    if s2 != "**" {
                        return Err(Error::Expecting(
                            "**".to_string(),
                            span.subset(s2.chars().count()),
                        ));
                    }
                    span.inc(2);
                    match s1 {
                        "ipn" => Ok(EidPatternItem::IpnPatternItem(IpnPatternItem::new_any())),
                        "dtn" => Ok(EidPatternItem::DtnPatternItem(DtnPatternItem::new_any())),
                        _ => Ok(EidPatternItem::AnyTextScheme(s1.to_string())),
                    }
                }
                _ => Err(Error::InvalidScheme(span.subset(s1.chars().count()))),
            },
        }
    }
}
