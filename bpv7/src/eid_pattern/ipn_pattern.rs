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
        if s == "**" {
            return Ok(IpnPatternItem::new_any());
        }

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

impl std::fmt::Display for IpnPatternItem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}.{}.{}",
            self.allocator_id, self.node_number, self.service_number
        )
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
        match s {
            "*" => {
                span.inc(1);
                Ok(IpnPattern::Wildcard)
            }
            "0" => {
                span.inc(1);
                Ok(IpnPattern::Range(vec![IpnInterval::Number(0)]))
            }
            _ => match s.chars().nth(0) {
                Some('1'..='9') => {
                    let Ok(v) = s.parse() else {
                        return Err(EidPatternError::InvalidIpnNumber(
                            span.subset(s.chars().count()),
                        ));
                    };
                    span.inc(s.chars().count());
                    Ok(IpnPattern::Range(vec![IpnInterval::Number(v)]))
                }
                Some('[') => {
                    let Some(s) = s[1..].strip_suffix(']') else {
                        span.offset(s.chars().count() - 1);
                        return Err(EidPatternError::Expecting("]".to_string(), span.subset(1)));
                    };
                    span.inc(1);

                    // Parse intervals
                    let mut intervals = s.split(',').try_fold(Vec::new(), |mut v, s| {
                        v.push(IpnInterval::parse(s, span)?);
                        Ok::<Vec<IpnInterval>, EidPatternError>(v)
                    })?;

                    if intervals.is_empty() {
                        Err(EidPatternError::InvalidIpnNumber(
                            span.subset(s.chars().count()),
                        ))
                    } else {
                        // Sort
                        intervals.sort();

                        // Dedup
                        intervals.dedup();

                        // Merge intervals
                        let mut i = intervals.into_iter();
                        let mut intervals = Vec::new();
                        let mut curr = i.next().unwrap();
                        for next in i {
                            match (&curr, &next) {
                                (IpnInterval::Number(n1), IpnInterval::Number(n2))
                                    if *n2 == n1 + 1 =>
                                {
                                    curr = IpnInterval::Range(*n1..=*n2);
                                }
                                (IpnInterval::Number(n), IpnInterval::Range(r))
                                    if n == r.start() =>
                                {
                                    curr = next;
                                }
                                (IpnInterval::Number(n), IpnInterval::Range(r))
                                    if n + 1 == *r.start() =>
                                {
                                    curr = IpnInterval::Range(*n..=*r.end());
                                }
                                (IpnInterval::Range(r), IpnInterval::Number(n))
                                    if r.contains(n) => {}
                                (IpnInterval::Range(r), IpnInterval::Number(n))
                                    if r.end() + 1 == *n =>
                                {
                                    curr = IpnInterval::Range(*r.start()..=*n);
                                }
                                (IpnInterval::Range(r1), IpnInterval::Range(r2))
                                    if *r2.start() <= r1.end() + 1 =>
                                {
                                    curr = IpnInterval::Range(*r1.start()..=*r2.end());
                                }
                                _ => {
                                    intervals.push(curr);
                                    curr = next;
                                }
                            }
                        }
                        intervals.push(curr);

                        span.inc(1);
                        Ok(IpnPattern::Range(intervals))
                    }
                }
                _ => Err(EidPatternError::InvalidIpnNumber(
                    span.subset(s.chars().count()),
                )),
            },
        }
    }
}

impl std::fmt::Display for IpnPattern {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            IpnPattern::Range(r) if r.len() == 1 => match r[0] {
                IpnInterval::Number(n) => write!(f, "{n}"),
                IpnInterval::Range(_) => write!(f, "[{}]", r[0]),
            },
            IpnPattern::Range(r) => {
                write!(f, "[")?;
                for (i, r) in r.iter().enumerate() {
                    if i > 0 {
                        write!(f, ",")?;
                    }
                    write!(f, "{r}")?;
                }
                write!(f, "]")
            }
            IpnPattern::Wildcard => write!(f, "*"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum IpnInterval {
    Number(u32),
    Range(RangeInclusive<u32>),
}

impl std::fmt::Display for IpnInterval {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            IpnInterval::Number(n) => write!(f, "{n}"),
            IpnInterval::Range(r) => write!(f, "{}-{}", r.start(), r.end()),
        }
    }
}

impl std::cmp::PartialOrd for IpnInterval {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl std::cmp::Ord for IpnInterval {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        match (self, other) {
            (IpnInterval::Number(n1), IpnInterval::Number(n2)) => n1.cmp(n2),
            (IpnInterval::Number(n), IpnInterval::Range(r)) => {
                n.cmp(r.start()).then((r.end() - r.start()).cmp(&1))
            }
            (IpnInterval::Range(r), IpnInterval::Number(n)) => {
                r.start().cmp(n).then(1.cmp(&(r.end() - r.start())))
            }
            (IpnInterval::Range(r1), IpnInterval::Range(r2)) => r1
                .start()
                .cmp(r2.start())
                .then((r1.end() - r1.start()).cmp(&(r2.end() - r2.start()))),
        }
    }
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
                Ok(IpnInterval::Range(start..=end))
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
                let Ok(v) = s.parse() else {
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
