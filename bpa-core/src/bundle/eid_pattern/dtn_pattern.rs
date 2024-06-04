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
            singles: Vec::new(),
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
    pub fn parse(s: &str, span: &mut Span) -> Result<Self, Error> {
        if s == "none" {
            span.inc(4);
            Ok(DtnPatternItem::None)
        } else {
            Ok(DtnPatternItem::DtnSsp(DtnSsp::parse(s, span)?))
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DtnSsp {
    pub authority: DtnAuthPattern,
    pub singles: Vec<DtnSinglePattern>,
    pub last: DtnLastPattern,
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

fn url_decode(s: &str, span: &mut Span) -> Result<String, Error> {
    urlencoding::decode(s)
        .map_err(|e| Error::InvalidUtf8(e, span.subset(s.chars().count())))
        .map(|s2| {
            span.inc(s.chars().count());
            s2.into_owned()
        })
}

#[derive(Debug, Clone)]
pub enum PatternMatch {
    Exact(String),
    Regex(regex::Regex),
}

impl PatternMatch {
    fn is_match(&self, s: &str) -> bool {
        match self {
            PatternMatch::Exact(e) => e == s,
            PatternMatch::Regex(r) => r.is_match(s),
        }
    }

    fn is_exact(&self) -> Option<String> {
        match self {
            PatternMatch::Exact(s) => Some(s.clone()),
            PatternMatch::Regex(_) => None,
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
                        PatternMatch::Regex(r)
                    })
            }
        } else {
            Ok(PatternMatch::Exact(url_decode(s, span)?))
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

    fn is_exact(&self) -> Option<String> {
        match self {
            DtnLastPattern::Single(p) => p.is_exact(),
            DtnLastPattern::MultiWildcard => None,
        }
    }

    /*
    dtn-last-pat = dtn-single-pat / multi-wildcard
    */
    fn parse(s: &str, span: &mut Span) -> Result<Self, Error> {
        if s.is_empty() {
            Ok(DtnLastPattern::Single(DtnSinglePattern::PatternMatch(
                PatternMatch::Exact("".to_string()),
            )))
        } else if s == "**" {
            span.inc(2);
            Ok(DtnLastPattern::MultiWildcard)
        } else {
            Ok(DtnLastPattern::Single(DtnSinglePattern::parse(s, span)?))
        }
    }
}
