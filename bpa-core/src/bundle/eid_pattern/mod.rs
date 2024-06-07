use super::*;

mod dtn_pattern;
mod error;
mod ipn_pattern;

#[cfg(test)]
mod str_tests;

use error::Span;

pub use dtn_pattern::*;
pub use error::EidPatternError;
pub use ipn_pattern::*;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
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
    type Err = EidPatternError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s == "*:**" {
            Ok(EidPattern::Any)
        } else {
            let mut v = Vec::new();
            let mut span = Span::new(1, 1);
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
            Eid::Unknown { scheme, .. } => {
                EidPattern::Set(vec![EidPatternItem::AnyNumericScheme(scheme)])
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
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

    pub fn is_exact(&self) -> Option<Eid> {
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
    fn parse(s: &str, span: &mut Span) -> Result<Self, EidPatternError> {
        let Some((s1, s2)) = s.split_once(':') else {
            return Err(EidPatternError::Expecting(
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
                        return Err(EidPatternError::InvalidScheme(
                            span.subset(s1.chars().count()),
                        ));
                    };

                    if v == 0 {
                        return Err(EidPatternError::InvalidScheme(
                            span.subset(s1.chars().count()),
                        ));
                    }

                    span.inc(s1.chars().count() + 1);
                    if s2 != "**" {
                        return Err(EidPatternError::Expecting(
                            "**".to_string(),
                            span.subset(s2.chars().count()),
                        ));
                    }
                    span.inc(2);
                    match v {
                        1 => Ok(EidPatternItem::DtnPatternItem(DtnPatternItem::new_any())),
                        2 => Ok(EidPatternItem::IpnPatternItem(IpnPatternItem::new_any())),
                        _ => Ok(EidPatternItem::AnyNumericScheme(v)),
                    }
                }
                Some('A'..='Z') | Some('a'..='z') => {
                    for c in s1.chars() {
                        if !matches!(c,'A'..='Z' | 'a'..='z' | '0'..='9' | '+' | '-' | '.') {
                            return Err(EidPatternError::InvalidScheme(
                                span.subset(s1.chars().count()),
                            ));
                        }
                        span.inc(1);
                    }

                    span.inc(1);
                    if s2 != "**" {
                        return Err(EidPatternError::Expecting(
                            "**".to_string(),
                            span.subset(s2.chars().count()),
                        ));
                    }
                    span.inc(2);
                    Ok(EidPatternItem::AnyTextScheme(s1.to_string()))
                }
                _ => Err(EidPatternError::InvalidScheme(
                    span.subset(s1.chars().count()),
                )),
            },
        }
    }
}
