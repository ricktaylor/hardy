use super::*;
use std::ops::RangeInclusive;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct IpnPatternItem {
    pub allocator_id: IpnPattern,
    pub node_number: IpnPattern,
    pub service_number: IpnPattern,
}

impl IpnPatternItem {
    pub fn new_any() -> Self {
        Self {
            allocator_id: IpnPattern::Wildcard,
            node_number: IpnPattern::Wildcard,
            service_number: IpnPattern::Wildcard,
        }
    }
    pub fn is_match(&self, eid: &Eid) -> bool {
        match eid {
            Eid::Null => {
                self.allocator_id.is_match(0)
                    && self.node_number.is_match(0)
                    && self.service_number.is_match(0)
            }
            Eid::LocalNode { service_number } => {
                self.allocator_id.is_match(0)
                    && self.node_number.is_match(u32::MAX)
                    && self.service_number.is_match(*service_number)
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
            } => {
                self.allocator_id.is_match(*allocator_id)
                    && self.node_number.is_match(*node_number)
                    && self.service_number.is_match(*service_number)
            }
            _ => false,
        }
    }

    pub fn is_exact(&self) -> Option<Eid> {
        Some(Eid::Ipn3 {
            allocator_id: self.allocator_id.is_exact()?,
            node_number: self.node_number.is_exact()?,
            service_number: self.service_number.is_exact()?,
        })
    }

    /*
    ipn-ssp = ipn-part-pat nbr-delim ipn-part-pat nbr-delim ipn-part-pat
    */
    pub fn parse(s: &str, span: &mut Span) -> Result<Self, EidPatternError> {
        let Some((s1, s)) = s.split_once('.') else {
            IpnPattern::parse(s, span)?;
            return Err(EidPatternError::Expecting(".".to_string(), span.clone()));
        };

        let allocator_id = IpnPattern::parse(s1, span)?;
        span.inc(1);

        let Some((s1, s)) = s.split_once('.') else {
            IpnPattern::parse(s, span)?;
            return Err(EidPatternError::Expecting(".".to_string(), span.clone()));
        };

        let node_number = IpnPattern::parse(s1, span)?;
        span.inc(1);

        Ok(IpnPatternItem {
            allocator_id,
            node_number,
            service_number: IpnPattern::parse(s, span)?,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum IpnPattern {
    Range(Vec<IpnInterval>),
    Wildcard,
}

impl IpnPattern {
    fn is_match(&self, v: u32) -> bool {
        match self {
            IpnPattern::Range(r) => r.iter().any(|r| r.is_match(v)),
            IpnPattern::Wildcard => true,
        }
    }

    fn is_exact(&self) -> Option<u32> {
        match self {
            IpnPattern::Range(r) => {
                if r.len() != 1 {
                    None
                } else {
                    r[0].is_exact()
                }
            }
            IpnPattern::Wildcard => None,
        }
    }

    /*
    ipn-part-pat = ipn-number / ipn-range / wildcard
    ipn-number = "0" / non-zero-number
    ipn-range = "[" ipn-interval *( "," ipn-interval ) "]"
    */
    fn parse(s: &str, span: &mut Span) -> Result<Self, EidPatternError> {
        match s.chars().nth(0) {
            Some('0') => {
                if s.len() > 1 {
                    return Err(EidPatternError::InvalidIpnNumber(
                        span.subset(s.chars().count()),
                    ));
                }
                span.inc(1);
                Ok(IpnPattern::Range(vec![IpnInterval::Number(0)]))
            }
            Some('1'..='9') => {
                let Ok(v) = s.parse::<u32>() else {
                    return Err(EidPatternError::InvalidIpnNumber(
                        span.subset(s.chars().count()),
                    ));
                };
                span.inc(s.chars().count());
                Ok(IpnPattern::Range(vec![IpnInterval::Number(v)]))
            }
            Some('[') => {
                if !s.ends_with(']') {
                    span.offset(s.chars().count() - 1);
                    return Err(EidPatternError::Expecting("]".to_string(), span.subset(1)));
                }

                span.inc(1);
                Ok(IpnPattern::Range(s[1..s.len() - 1].split(',').try_fold(
                    Vec::new(),
                    |mut v, s| {
                        v.push(IpnInterval::parse(s, span)?);
                        span.inc(1);
                        Ok::<Vec<IpnInterval>, EidPatternError>(v)
                    },
                )?))
            }
            _ => Err(EidPatternError::InvalidIpnNumber(
                span.subset(s.chars().count()),
            )),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum IpnInterval {
    Number(u32),
    Range(RangeInclusive<u32>),
}

impl IpnInterval {
    fn is_match(&self, v: u32) -> bool {
        match self {
            IpnInterval::Number(n) => *n == v,
            IpnInterval::Range(r) => r.contains(&v),
        }
    }

    fn is_exact(&self) -> Option<u32> {
        match self {
            IpnInterval::Number(n) => Some(*n),
            IpnInterval::Range(_) => None,
        }
    }

    /*
    ipn-interval = ipn-number [ "-" ipn-number ]
    */
    fn parse(s: &str, span: &mut Span) -> Result<Self, EidPatternError> {
        if let Some((s1, s2)) = s.split_once('-') {
            let start = Self::parse_number(s1, span)?;
            span.inc(1);
            let end = Self::parse_number(s2, span)?;

            if start == end {
                Ok(IpnInterval::Number(start))
            } else {
                // Inclusive range!
                Ok(IpnInterval::Range(RangeInclusive::new(start, end)))
            }
        } else {
            Ok(IpnInterval::Number(Self::parse_number(s, span)?))
        }
    }

    /*
    ipn-number = "0" / non-zero-number
    */
    fn parse_number(s: &str, span: &mut Span) -> Result<u32, EidPatternError> {
        match s.chars().nth(0) {
            Some('0') => {
                if s.len() > 1 {
                    return Err(EidPatternError::InvalidIpnNumber(
                        span.subset(s.chars().count()),
                    ));
                }
                span.inc(1);
                Ok(0)
            }
            Some('1'..='9') => {
                let Ok(v) = s.parse::<u32>() else {
                    return Err(EidPatternError::InvalidIpnNumber(
                        span.subset(s.chars().count()),
                    ));
                };
                span.inc(s.chars().count());
                Ok(v)
            }
            _ => Err(EidPatternError::InvalidIpnNumber(
                span.subset(s.chars().count()),
            )),
        }
    }
}
