use super::*;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum DtnPatternItem {
    DtnSsp(DtnSsp),
    None,
}

impl DtnPatternItem {
    pub fn new_any() -> Self {
        DtnPatternItem::DtnSsp(DtnSsp {
            authority: DtnAuthPattern::MultiWildcard,
            singles: [].into(),
            last: DtnLastPattern::MultiWildcard,
        })
    }

    pub fn is_match(&self, eid: &Eid) -> bool {
        match self {
            DtnPatternItem::None => matches!(eid, Eid::Null),
            DtnPatternItem::DtnSsp(s) => s.is_match(eid),
        }
    }

    pub fn is_exact(&self) -> Option<Eid> {
        match self {
            DtnPatternItem::None => Some(Eid::Null),
            DtnPatternItem::DtnSsp(s) => s.is_exact(),
        }
    }

    /*
    dtn-ssp = dtn-wkssp-exact / dtn-fullssp
    dtn-wkssp-exact = "none"
    */
    pub fn parse(s: &str, span: &mut Span) -> Result<Self, EidPatternError> {
        match s {
            "**" => {
                span.inc(2);
                Ok(DtnPatternItem::new_any())
            }
            "none" => {
                span.inc(4);
                Ok(DtnPatternItem::None)
            }
            _ => Ok(DtnPatternItem::DtnSsp(DtnSsp::parse(s, span)?)),
        }
    }
}

impl std::fmt::Display for DtnPatternItem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DtnPatternItem::None => write!(f, "none"),
            DtnPatternItem::DtnSsp(s) => write!(f, "{s}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DtnSsp {
    pub authority: DtnAuthPattern,
    pub singles: Box<[DtnSinglePattern]>,
    pub last: DtnLastPattern,
}

impl DtnSsp {
    fn is_match(&self, eid: &Eid) -> bool {
        let Eid::Dtn { node_name, demux } = eid else {
            return false;
        };

        match self.authority.is_match(node_name) {
            (false, _) => return false,
            (true, false) => return true,
            _ => {}
        }

        let mut demux = demux.iter();
        for s in &self.singles {
            let Some(next) = demux.next() else {
                return false;
            };

            if !s.is_match(next) {
                return false;
            }
        }

        let Some(last) = demux.next() else {
            return false;
        };
        match self.last.is_match(last) {
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

        Some(Eid::Dtn {
            node_name,
            demux: demux.into(),
        })
    }

    /*
    dtn-fullssp = "//" dtn-authority-pat "/" dtn-path-pat
    dtn-authority-pat = exact / regexp / multi-wildcard
    dtn-path-pat = *( dtn-single-pat "/" ) dtn-last-pat
    dtn-single-pat = exact / regexp / wildcard
    dtn-last-pat = dtn-single-pat / multi-wildcard
    */
    fn parse(s: &str, span: &mut Span) -> Result<Self, EidPatternError> {
        let Some(s) = s.strip_prefix("//") else {
            return Err(EidPatternError::Expecting(
                "//".to_string(),
                span.subset(s.chars().count().min(2)),
            ));
        };
        span.offset(2);

        let Some((s1, s2)) = s.split_once('/') else {
            return Err(EidPatternError::Expecting(
                "/".to_string(),
                span.subset(s.chars().count()),
            ));
        };

        let authority = DtnAuthPattern::parse(s1, span)?;

        span.inc(1);

        let mut parts = s2.split('/');
        let Some(last) = parts.nth_back(0) else {
            return Err(EidPatternError::Expecting(
                "/".to_string(),
                span.subset(s2.chars().count()),
            ));
        };

        let singles = parts.try_fold(Vec::new(), |mut v, s| {
            v.push(DtnSinglePattern::parse(s, span)?);
            span.inc(1);
            Ok::<Vec<DtnSinglePattern>, EidPatternError>(v)
        })?;

        Ok(DtnSsp {
            authority,
            singles: singles.into(),
            last: DtnLastPattern::parse(last, span)?,
        })
    }
}

impl std::fmt::Display for DtnSsp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "//{}", self.authority)?;
        for s in &self.singles {
            write!(f, "/{s}")?;
        }
        write!(f, "/{}", self.last)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum DtnAuthPattern {
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

    fn is_exact(&self) -> Option<Box<str>> {
        match self {
            DtnAuthPattern::PatternMatch(p) => p.is_exact(),
            DtnAuthPattern::MultiWildcard => None,
        }
    }

    /*
    dtn-authority-pat = exact / regexp / multi-wildcard
    */
    fn parse(s: &str, span: &mut Span) -> Result<Self, EidPatternError> {
        if s == "**" {
            span.inc(2);
            Ok(DtnAuthPattern::MultiWildcard)
        } else {
            Ok(DtnAuthPattern::PatternMatch(PatternMatch::parse(s, span)?))
        }
    }
}

impl std::fmt::Display for DtnAuthPattern {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DtnAuthPattern::PatternMatch(p) => write!(f, "{p}"),
            DtnAuthPattern::MultiWildcard => write!(f, "**"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum DtnSinglePattern {
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

    fn is_exact(&self) -> Option<Box<str>> {
        match self {
            DtnSinglePattern::PatternMatch(p) => p.is_exact(),
            DtnSinglePattern::Wildcard => None,
        }
    }

    /*
    dtn-single-pat = exact / regexp / wildcard
    */
    fn parse(s: &str, span: &mut Span) -> Result<Self, EidPatternError> {
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

impl std::fmt::Display for DtnSinglePattern {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DtnSinglePattern::PatternMatch(p) => write!(f, "{p}"),
            DtnSinglePattern::Wildcard => write!(f, "*"),
        }
    }
}

fn url_decode(s: &str, span: &mut Span) -> Result<Box<str>, EidPatternError> {
    urlencoding::decode(s)
        .map_err(|e| EidPatternError::InvalidUtf8(e, span.subset(s.chars().count())))
        .map(|s2| {
            span.inc(s.chars().count());
            s2.into()
        })
}

#[derive(Debug, Clone)]
pub enum PatternMatch {
    Exact(Box<str>),
    Regex(regex::Regex),
}

impl PatternMatch {
    fn is_match(&self, s: &str) -> bool {
        match self {
            PatternMatch::Exact(e) => **e == *s,
            PatternMatch::Regex(r) => r.is_match(s),
        }
    }

    fn is_exact(&self) -> Option<Box<str>> {
        match self {
            PatternMatch::Exact(s) => Some(s.clone()),
            PatternMatch::Regex(_) => None,
        }
    }

    fn parse(s: &str, span: &mut Span) -> Result<Self, EidPatternError> {
        if s.starts_with('[') {
            if !s.ends_with(']') {
                span.offset(s.chars().count() - 1);
                Err(EidPatternError::Expecting("]".to_string(), span.subset(1)))
            } else if s.len() == 2 {
                Err(EidPatternError::ExpectingRegEx(
                    span.subset(s.chars().count()),
                ))
            } else {
                span.inc(1);

                regex::Regex::new(&url_decode(&s[1..s.len() - 1], &mut span.clone())?)
                    .map_err(|e| {
                        EidPatternError::InvalidRegEx(e, span.subset(s.chars().count() - 1))
                    })
                    .map(|r| {
                        span.inc(s.chars().count() - 1);
                        PatternMatch::Regex(r)
                    })
            }
        } else {
            Ok(PatternMatch::Exact(url_decode(s, span)?))
        }
    }
}

impl std::fmt::Display for PatternMatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PatternMatch::Exact(s) => write!(f, "{s}"),
            PatternMatch::Regex(r) => write!(f, "[{}]", r.as_str()),
        }
    }
}

impl std::cmp::PartialEq for PatternMatch {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Exact(l), Self::Exact(r)) => l == r,
            (Self::Regex(l), Self::Regex(r)) => l.as_str() == r.as_str(),
            _ => false,
        }
    }
}

impl std::cmp::Eq for PatternMatch {}

impl std::hash::Hash for PatternMatch {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        core::mem::discriminant(self).hash(state);
        match self {
            PatternMatch::Exact(s) => s.hash(state),
            PatternMatch::Regex(r) => r.as_str().hash(state),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum DtnLastPattern {
    Single(DtnSinglePattern),
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

    fn is_exact(&self) -> Option<Box<str>> {
        match self {
            DtnLastPattern::Single(p) => p.is_exact(),
            DtnLastPattern::MultiWildcard => None,
        }
    }

    /*
    dtn-last-pat = dtn-single-pat / multi-wildcard
    */
    fn parse(s: &str, span: &mut Span) -> Result<Self, EidPatternError> {
        if s.is_empty() {
            Ok(DtnLastPattern::Single(DtnSinglePattern::PatternMatch(
                PatternMatch::Exact("".into()),
            )))
        } else if s == "**" {
            span.inc(2);
            Ok(DtnLastPattern::MultiWildcard)
        } else {
            Ok(DtnLastPattern::Single(DtnSinglePattern::parse(s, span)?))
        }
    }
}

impl std::fmt::Display for DtnLastPattern {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DtnLastPattern::Single(p) => write!(f, "{p}"),
            DtnLastPattern::MultiWildcard => write!(f, "**"),
        }
    }
}
