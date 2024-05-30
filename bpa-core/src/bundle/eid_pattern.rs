use super::*;
use std::ops::{Range, RangeInclusive};
use thiserror::Error;

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

fn url_decode(s: &str, span: &mut Span) -> Result<String, Error> {
    urlencoding::decode(s)
        .map_err(|e| Error::InvalidUtf8(e, span.subset(s.chars().count())))
        .map(|s2| {
            span.inc(s.chars().count());
            s2.into_owned()
        })
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

#[derive(Debug)]
pub enum DtnPatternItem {
    DtnSsp(DtnSsp),
    None,
}

impl DtnPatternItem {
    fn new_any() -> Self {
        DtnPatternItem::DtnSsp(DtnSsp {
            authority: DtnAuthPattern::MultiWildcard,
            singles: Vec::new(),
            last: DtnLastPattern::MultiWildcard,
        })
    }

    fn is_match(&self, eid: &Eid) -> bool {
        match self {
            DtnPatternItem::None => matches!(eid, Eid::Null),
            DtnPatternItem::DtnSsp(s) => s.is_match(eid),
        }
    }

    fn is_exact(&self) -> Option<Eid> {
        match self {
            DtnPatternItem::None => Some(Eid::Null),
            DtnPatternItem::DtnSsp(s) => s.is_exact(),
        }
    }

    /*
    dtn-ssp = dtn-wkssp-exact / dtn-fullssp
    dtn-wkssp-exact = "none"
    */
    fn parse(s: &str, span: &mut Span) -> Result<Self, Error> {
        if s == "none" {
            span.inc(4);
            Ok(DtnPatternItem::None)
        } else {
            Ok(DtnPatternItem::DtnSsp(DtnSsp::parse(s, span)?))
        }
    }
}

#[derive(Debug)]
pub struct DtnSsp {
    authority: DtnAuthPattern,
    singles: Vec<DtnSinglePattern>,
    last: DtnLastPattern,
}

impl DtnSsp {
    fn is_match(&self, eid: &Eid) -> bool {
        let Eid::Dtn { node_name, demux } = eid else {
            return false;
        };

        match self.authority.is_match(node_name.as_str()) {
            (false, _) => return false,
            (true, false) => return true,
            _ => {}
        }

        let mut demux = demux.iter();
        for s in &self.singles {
            let Some(next) = demux.next() else {
                return false;
            };

            if !s.is_match(next.as_str()) {
                return false;
            }
        }

        let Some(last) = demux.next() else {
            return false;
        };
        match self.last.is_match(last.as_str()) {
            (true, true) => demux.next().is_none(),
            (true, false) => true,
            (false, _) => false,
        }
    }

    fn is_exact(&self) -> Option<Eid> {
        let node_name = self.authority.is_exact()?;
        let mut demux = self.singles.iter().try_fold(Vec::new(), |mut v, s| {
            let s = s.is_exact()?;
            v.push(s);
            Some(v)
        })?;
        demux.push(self.last.is_exact()?);

        Some(Eid::Dtn { node_name, demux })
    }

    /*
    dtn-fullssp = "//" dtn-authority-pat "/" dtn-path-pat
    dtn-authority-pat = exact / regexp / multi-wildcard
    dtn-path-pat = *( dtn-single-pat "/" ) dtn-last-pat
    dtn-single-pat = exact / regexp / wildcard
    dtn-last-pat = dtn-single-pat / multi-wildcard
    */
    fn parse(s: &str, span: &mut Span) -> Result<Self, Error> {
        let Some(s) = s.strip_prefix("//") else {
            return Err(Error::Expecting(
                "//".to_string(),
                span.subset(s.chars().count().min(2)),
            ));
        };
        span.0.start += 2;
        span.0.end += 2;

        let Some((s1, s2)) = s.split_once('/') else {
            return Err(Error::Expecting(
                "/".to_string(),
                span.subset(s.chars().count()),
            ));
        };

        let authority = DtnAuthPattern::parse(s1, span)?;

        span.inc(1);

        let mut parts = s2.split('/');
        let Some(last) = parts.nth_back(0) else {
            return Err(Error::Expecting(
                "/".to_string(),
                span.subset(s2.chars().count()),
            ));
        };

        let singles = parts.try_fold(Vec::new(), |mut v, s| {
            v.push(DtnSinglePattern::parse(s, span)?);
            span.inc(1);
            Ok::<Vec<DtnSinglePattern>, Error>(v)
        })?;

        Ok(DtnSsp {
            authority,
            singles,
            last: DtnLastPattern::parse(last, span)?,
        })
    }
}

#[derive(Debug)]
enum DtnAuthPattern {
    PatternMatch(PatternMatch),
    MultiWildcard,
}

impl DtnAuthPattern {
    fn is_match(&self, s: &str) -> (bool, bool) {
        match self {
            DtnAuthPattern::PatternMatch(p) => (p.is_match(s), true),
            DtnAuthPattern::MultiWildcard => (true, false),
        }
    }

    fn is_exact(&self) -> Option<String> {
        match self {
            DtnAuthPattern::PatternMatch(p) => p.is_exact(),
            DtnAuthPattern::MultiWildcard => None,
        }
    }

    /*
    dtn-authority-pat = exact / regexp / multi-wildcard
    */
    fn parse(s: &str, span: &mut Span) -> Result<Self, Error> {
        if s == "**" {
            span.inc(2);
            Ok(DtnAuthPattern::MultiWildcard)
        } else {
            Ok(DtnAuthPattern::PatternMatch(PatternMatch::parse(s, span)?))
        }
    }
}

#[derive(Debug)]
enum DtnSinglePattern {
    PatternMatch(PatternMatch),
    Wildcard,
}

impl DtnSinglePattern {
    fn is_match(&self, s: &str) -> bool {
        match self {
            DtnSinglePattern::PatternMatch(p) => p.is_match(s),
            DtnSinglePattern::Wildcard => true,
        }
    }

    fn is_exact(&self) -> Option<String> {
        match self {
            DtnSinglePattern::PatternMatch(p) => p.is_exact(),
            DtnSinglePattern::Wildcard => None,
        }
    }

    /*
    dtn-single-pat = exact / regexp / wildcard
    */
    fn parse(s: &str, span: &mut Span) -> Result<Self, Error> {
        if s == "*" {
            span.inc(1);
            Ok(DtnSinglePattern::Wildcard)
        } else {
            Ok(DtnSinglePattern::PatternMatch(PatternMatch::parse(
                s, span,
            )?))
        }
    }
}

#[derive(Debug)]
enum PatternMatch {
    Exact(String),
    RegExp(regex::Regex),
}

impl PatternMatch {
    fn is_match(&self, s: &str) -> bool {
        match self {
            PatternMatch::Exact(e) => e == s,
            PatternMatch::RegExp(r) => r.is_match(s),
        }
    }

    fn is_exact(&self) -> Option<String> {
        match self {
            PatternMatch::Exact(s) => Some(s.clone()),
            PatternMatch::RegExp(_) => None,
        }
    }

    fn parse(s: &str, span: &mut Span) -> Result<Self, Error> {
        if s.starts_with('[') {
            if !s.ends_with(']') {
                Err(Error::Expecting(
                    "]".to_string(),
                    Span::new(
                        span.0.start + s.chars().count() - 1,
                        span.0.start + s.chars().count(),
                    ),
                ))
            } else if s.len() == 2 {
                Err(Error::ExpectingRegEx(span.subset(s.chars().count())))
            } else {
                span.inc(1);

                regex::Regex::new(url_decode(&s[1..s.len() - 1], &mut span.clone())?.as_str())
                    .map_err(|e| Error::InvalidRegEx(e, span.subset(s.chars().count() - 1)))
                    .map(|r| {
                        span.inc(s.chars().count() - 1);
                        PatternMatch::RegExp(r)
                    })
            }
        } else {
            Ok(PatternMatch::Exact(url_decode(s, span)?))
        }
    }
}

#[derive(Debug)]
enum DtnLastPattern {
    Single(DtnSinglePattern),
    Empty,
    MultiWildcard,
}

impl DtnLastPattern {
    fn is_match(&self, s: &str) -> (bool, bool) {
        if let DtnLastPattern::Single(p) = self {
            (p.is_match(s), true)
        } else {
            (true, false)
        }
    }

    fn is_exact(&self) -> Option<String> {
        match self {
            DtnLastPattern::Single(p) => p.is_exact(),
            DtnLastPattern::Empty => Some("".to_string()),
            DtnLastPattern::MultiWildcard => None,
        }
    }

    /*
    dtn-last-pat = dtn-single-pat / multi-wildcard
    */
    fn parse(s: &str, span: &mut Span) -> Result<Self, Error> {
        if s.is_empty() {
            Ok(DtnLastPattern::Empty)
        } else if s == "**" {
            span.inc(2);
            Ok(DtnLastPattern::MultiWildcard)
        } else {
            Ok(DtnLastPattern::Single(DtnSinglePattern::parse(s, span)?))
        }
    }
}

#[derive(Debug)]
pub struct IpnPatternItem {
    pub allocator_id: IpnPattern,
    pub node_number: IpnPattern,
    pub service_number: IpnPattern,
}

impl IpnPatternItem {
    fn new_any() -> Self {
        Self {
            allocator_id: IpnPattern::Wildcard,
            node_number: IpnPattern::Wildcard,
            service_number: IpnPattern::Wildcard,
        }
    }
    fn is_match(&self, eid: &Eid) -> bool {
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

    fn is_exact(&self) -> Option<Eid> {
        Some(Eid::Ipn3 {
            allocator_id: self.allocator_id.is_exact()?,
            node_number: self.node_number.is_exact()?,
            service_number: self.service_number.is_exact()?,
        })
    }

    /*
    ipn-ssp = ipn-part-pat nbr-delim ipn-part-pat nbr-delim ipn-part-pat
    */
    fn parse(s: &str, span: &mut Span) -> Result<Self, Error> {
        let Some((s1, s)) = s.split_once('.') else {
            IpnPattern::parse(s, span)?;
            return Err(Error::Expecting(".".to_string(), span.clone()));
        };

        let allocator_id = IpnPattern::parse(s1, span)?;
        span.inc(1);

        let Some((s1, s)) = s.split_once('.') else {
            IpnPattern::parse(s, span)?;
            return Err(Error::Expecting(".".to_string(), span.clone()));
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

#[derive(Debug)]
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
    fn parse(s: &str, span: &mut Span) -> Result<Self, Error> {
        match s.chars().nth(0) {
            Some('0') => {
                if s.len() > 1 {
                    return Err(Error::InvalidIpnNumber(span.subset(s.chars().count())));
                }
                span.inc(1);
                Ok(IpnPattern::Range(vec![IpnInterval::Number(0)]))
            }
            Some('1'..='9') => {
                let Ok(v) = s.parse::<u32>() else {
                    return Err(Error::InvalidIpnNumber(span.subset(s.chars().count())));
                };
                span.inc(s.chars().count());
                Ok(IpnPattern::Range(vec![IpnInterval::Number(v)]))
            }
            Some('[') => {
                if !s.ends_with(']') {
                    return Err(Error::Expecting(
                        "]".to_string(),
                        Span::new(
                            span.0.start + s.chars().count() - 1,
                            span.0.start + s.chars().count(),
                        ),
                    ));
                }

                span.inc(1);
                Ok(IpnPattern::Range(s[1..s.len() - 1].split(',').try_fold(
                    Vec::new(),
                    |mut v, s| {
                        v.push(IpnInterval::parse(s, span)?);
                        span.inc(1);
                        Ok::<Vec<IpnInterval>, Error>(v)
                    },
                )?))
            }
            _ => Err(Error::InvalidIpnNumber(span.subset(s.chars().count()))),
        }
    }
}

#[derive(Debug)]
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
            IpnInterval::Range(_) => todo!(),
        }
    }

    /*
    ipn-interval = ipn-number [ "-" ipn-number ]
    */
    fn parse(s: &str, span: &mut Span) -> Result<Self, Error> {
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
    fn parse_number(s: &str, span: &mut Span) -> Result<u32, Error> {
        match s.chars().nth(0) {
            Some('0') => {
                if s.len() > 1 {
                    return Err(Error::InvalidIpnNumber(span.subset(s.chars().count())));
                }
                span.inc(1);
                Ok(0)
            }
            Some('1'..='9') => {
                let Ok(v) = s.parse::<u32>() else {
                    return Err(Error::InvalidIpnNumber(span.subset(s.chars().count())));
                };
                span.inc(s.chars().count());
                Ok(v)
            }
            _ => Err(Error::InvalidIpnNumber(span.subset(s.chars().count()))),
        }
    }
}
