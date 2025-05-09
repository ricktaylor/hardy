use super::*;
use std::borrow::Cow;
use winnow::{
    ModalResult, Parser,
    combinator::{alt, delimited, preceded, repeat, terminated},
    stream::AsChar,
    token::{one_of, take_while},
};

#[derive(Debug, Clone)]
pub struct HashableRegEx(regex::Regex);

impl HashableRegEx {
    pub fn try_new(v: &str) -> Result<Self, regex::Error> {
        Ok(Self(regex::RegexBuilder::new(v).build()?))
    }

    pub fn is_match(&self, haystack: &str) -> bool {
        self.0.is_match(haystack)
    }
}

impl std::cmp::PartialEq for HashableRegEx {
    fn eq(&self, other: &Self) -> bool {
        self.0.as_str() == other.0.as_str()
    }
}

impl std::cmp::Eq for HashableRegEx {}

impl std::cmp::PartialOrd for HashableRegEx {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl std::cmp::Ord for HashableRegEx {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.0.as_str().cmp(other.0.as_str())
    }
}

impl std::hash::Hash for HashableRegEx {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.0.as_str().hash(state);
    }
}

impl std::fmt::Display for HashableRegEx {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum DtnPatternItem {
    DtnNone,
    DtnSsp(DtnSsp),
}

impl DtnPatternItem {
    pub(super) fn new_any() -> Self {
        Self::DtnSsp(DtnSsp {
            node_name: DtnNodeNamePattern::MultiWildcard,
            demux: [].into(),
            last_wild: true,
        })
    }

    pub(super) fn try_to_eid(&self) -> Option<Eid> {
        match self {
            DtnPatternItem::DtnNone => Some(Eid::Null),
            DtnPatternItem::DtnSsp(s) => s.try_to_eid(),
        }
    }
}

impl std::fmt::Display for DtnPatternItem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DtnPatternItem::DtnNone => write!(f, "none"),
            DtnPatternItem::DtnSsp(s) => write!(f, "{s}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DtnSsp {
    pub(crate) node_name: DtnNodeNamePattern,
    pub(crate) demux: Box<[DtnSinglePattern]>,
    pub(crate) last_wild: bool,
}

impl DtnSsp {
    pub(crate) fn new(node_name: Box<str>, demux: Box<[Box<str>]>, last_wild: bool) -> Self {
        Self {
            node_name: DtnNodeNamePattern::PatternMatch(PatternMatch::Exact(node_name)),
            demux: demux
                .into_iter()
                .map(|s| DtnSinglePattern::PatternMatch(PatternMatch::Exact(s)))
                .collect(),
            last_wild,
        }
    }

    fn try_to_eid(&self) -> Option<Eid> {
        if self.last_wild {
            return None;
        }

        Some(Eid::Dtn {
            node_name: self.node_name.try_to_str()?,
            demux: self
                .demux
                .iter()
                .try_fold(Vec::new(), |mut acc, s| {
                    acc.push(s.try_to_str()?);
                    Some(acc)
                })?
                .into(),
        })
    }
}

impl std::fmt::Display for DtnSsp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "//{}", self.node_name)?;
        for s in &self.demux {
            write!(f, "/{s}")?;
        }
        write!(f, "/{}", self.last_wild)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum DtnNodeNamePattern {
    PatternMatch(PatternMatch),
    MultiWildcard,
}

impl DtnNodeNamePattern {
    fn try_to_str(&self) -> Option<Box<str>> {
        match self {
            DtnNodeNamePattern::PatternMatch(p) => p.try_to_str(),
            DtnNodeNamePattern::MultiWildcard => None,
        }
    }
}

impl std::fmt::Display for DtnNodeNamePattern {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DtnNodeNamePattern::PatternMatch(p) => write!(f, "{p}"),
            DtnNodeNamePattern::MultiWildcard => write!(f, "**"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum DtnSinglePattern {
    PatternMatch(PatternMatch),
    Wildcard,
}

impl DtnSinglePattern {
    fn try_to_str(&self) -> Option<Box<str>> {
        match self {
            DtnSinglePattern::PatternMatch(p) => p.try_to_str(),
            DtnSinglePattern::Wildcard => None,
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

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum PatternMatch {
    Exact(Box<str>),
    Regex(HashableRegEx),
}

impl PatternMatch {
    fn try_to_str(&self) -> Option<Box<str>> {
        match self {
            PatternMatch::Exact(s) => Some(s.clone()),
            PatternMatch::Regex(_) => None,
        }
    }
}

impl std::fmt::Display for PatternMatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PatternMatch::Exact(s) => write!(f, "{s}"),
            PatternMatch::Regex(r) => write!(f, "[{r}]"),
        }
    }
}

// dtn-pat-item = "dtn:" dtn-ssp
pub(super) fn parse_dtn_pat_item(input: &mut &[u8]) -> ModalResult<EidPatternItem> {
    preceded(
        "dtn:",
        alt((
            "**".map(|_| EidPatternItem::DtnPatternItem(DtnPatternItem::new_any())),
            parse_dtn_ssp,
        )),
    )
    .parse_next(input)
}

// dtn-ssp = dtn-wkssp-exact / dtn-fullssp
// dtn-wkssp-exact = "none"
fn parse_dtn_ssp(input: &mut &[u8]) -> ModalResult<EidPatternItem> {
    alt((
        "none".map(|_| EidPatternItem::DtnPatternItem(DtnPatternItem::DtnNone)),
        parse_dtn_fullssp.map(|v| EidPatternItem::DtnPatternItem(DtnPatternItem::DtnSsp(v))),
    ))
    .parse_next(input)
}

// dtn-fullssp = "//" dtn-authority-pat "/" dtn-path-pat
fn parse_dtn_fullssp(input: &mut &[u8]) -> ModalResult<DtnSsp> {
    preceded(
        "//",
        (parse_dtn_authority_pat, preceded("/", parse_dtn_path_pat)).map(
            |(authority, (singles, last_wild))| DtnSsp {
                node_name: authority,
                demux: singles.into(),
                last_wild,
            },
        ),
    )
    .parse_next(input)
}

// dtn-authority-pat = exact / regexp / multi-wildcard
fn parse_dtn_authority_pat(input: &mut &[u8]) -> ModalResult<DtnNodeNamePattern> {
    alt((
        "**".map(|_| DtnNodeNamePattern::MultiWildcard),
        parse_regex.map(DtnNodeNamePattern::PatternMatch),
        parse_exact.map(DtnNodeNamePattern::PatternMatch),
    ))
    .parse_next(input)
}

// dtn-path-pat = *( dtn-single-pat "/" ) dtn-last-pat
fn parse_dtn_path_pat(input: &mut &[u8]) -> ModalResult<(Vec<DtnSinglePattern>, bool)> {
    (
        repeat(0.., terminated(parse_dtn_single_pat, "/")),
        parse_dtn_last_pat,
    )
        .map(
            |(mut a, b): (Vec<DtnSinglePattern>, Option<DtnSinglePattern>)| {
                if let Some(b) = b {
                    a.push(b);
                    (a, false)
                } else {
                    (a, true)
                }
            },
        )
        .parse_next(input)
}

// dtn-single-pat = exact / regexp / wildcard
fn parse_dtn_single_pat(input: &mut &[u8]) -> ModalResult<DtnSinglePattern> {
    alt((
        "*".map(|_| DtnSinglePattern::Wildcard),
        parse_regex.map(DtnSinglePattern::PatternMatch),
        parse_exact.map(DtnSinglePattern::PatternMatch),
    ))
    .parse_next(input)
}

// dtn-last-pat = dtn-single-pat / multi-wildcard
fn parse_dtn_last_pat(input: &mut &[u8]) -> ModalResult<Option<DtnSinglePattern>> {
    alt(("**".map(|_| None), parse_dtn_single_pat.map(Some))).parse_next(input)
}

fn from_hex_digit(digit: u8) -> u8 {
    match digit {
        b'0'..=b'9' => digit - b'0',
        b'A'..=b'F' => digit - b'A' + 10,
        _ => digit - b'a' + 10,
    }
}

// exact = *pchar
fn parse_exact(input: &mut &[u8]) -> ModalResult<PatternMatch> {
    repeat(
        0..,
        alt((
            take_while(
                1..,
                (
                    AsChar::is_alphanum,
                    '!',
                    '$',
                    '&'..='.',
                    ':',
                    ';',
                    '=',
                    '@',
                    '_',
                    '~',
                ),
            )
            .map(|v| Cow::Borrowed(unsafe { std::str::from_utf8_unchecked(v) })),
            preceded(
                '%',
                (one_of(AsChar::is_hex_digit), one_of(AsChar::is_hex_digit)),
            )
            .map(|(first, second)| {
                /* This is a more cautious UTF8 url decode expansion */
                let first = from_hex_digit(first);
                let second = from_hex_digit(second);
                if first <= 7 {
                    let val = [(first << 4) | second];
                    Cow::Owned(unsafe { std::str::from_utf8_unchecked(&val) }.into())
                } else {
                    let val = [0xC0u8 | (first >> 2), 0x80u8 | ((first & 3) << 4) | second];
                    Cow::Owned(unsafe { std::str::from_utf8_unchecked(&val) }.into())
                }
            }),
        )),
    )
    .fold(String::new, |mut acc, v| {
        if acc.is_empty() {
            acc = v.into()
        } else {
            acc.push_str(&v)
        }
        acc
    })
    .map(|v| PatternMatch::Exact(v.into()))
    .parse_next(input)
}

/*
; Regular expression for the whole SSP within the gen-delims brackets
; with an allowance for more regexp characters
regexp = "[" *( pchar / "^" ) "]"
*/
fn parse_regex(input: &mut &[u8]) -> ModalResult<PatternMatch> {
    delimited(
        "[",
        repeat(
            0..,
            alt((
                take_while(
                    1..,
                    (
                        AsChar::is_alphanum,
                        '!',
                        '$',
                        '&'..='.',
                        ':',
                        ';',
                        '=',
                        '@',
                        '^',
                        '_',
                        '~',
                    ),
                )
                .map(|v| Cow::Borrowed(unsafe { std::str::from_utf8_unchecked(v) })),
                preceded(
                    '%',
                    (one_of(AsChar::is_hex_digit), one_of(AsChar::is_hex_digit)),
                )
                .map(|(first, second)| {
                    /* This is a more cautious UTF8 url decode expansion */
                    let first = from_hex_digit(first);
                    let second = from_hex_digit(second);
                    if first <= 7 {
                        let val = [(first << 4) | second];
                        Cow::Owned(unsafe { std::str::from_utf8_unchecked(&val) }.into())
                    } else {
                        let val = [0xC0u8 | (first >> 2), 0x80u8 | ((first & 3) << 4) | second];
                        Cow::Owned(unsafe { std::str::from_utf8_unchecked(&val) }.into())
                    }
                }),
            )),
        )
        .fold(String::new, |mut acc, v| {
            if acc.is_empty() {
                acc = v.into()
            } else {
                acc.push_str(&v)
            }
            acc
        })
        .try_map(|v| Ok::<_, regex::Error>(PatternMatch::Regex(HashableRegEx::try_new(&v)?))),
        "]",
    )
    .parse_next(input)
}
