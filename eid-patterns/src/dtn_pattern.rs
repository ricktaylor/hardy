use super::*;
use winnow::{
    ModalResult, Parser,
    combinator::{alt, preceded, terminated},
    stream::AsChar,
    token::take_while,
};

// TODO:  The whole Glob thing needs more work.  Probably splitting into 2 parts, a node_name glob and demux glob
// Also need to ensure proper parsing of globs so we can have [|] an not interfere with set pipe splitting

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum DtnPatternItem {
    None,
    Any,
    Exact(Box<str>, Box<str>),
    Glob(glob::Pattern),
}

impl DtnPatternItem {
    pub(super) fn matches(&self, eid: &Eid) -> bool {
        match self {
            DtnPatternItem::None => eid.is_null(),
            DtnPatternItem::Any => matches!(eid, Eid::Dtn { .. } | Eid::Unknown { scheme: 1, .. }),
            DtnPatternItem::Exact(n1, d1) => {
                matches!(eid, Eid::Dtn { node_name, demux } if n1 == node_name && d1 == demux)
            }
            DtnPatternItem::Glob(pattern) => match eid {
                Eid::Dtn { node_name, demux } => do_glob(node_name, demux, pattern),
                _ => false,
            },
        }
    }

    pub(super) fn is_subset(&self, other: &Self) -> bool {
        match (self, other) {
            (DtnPatternItem::None, DtnPatternItem::None) => true,
            (DtnPatternItem::None, DtnPatternItem::Any) => false,
            (_, DtnPatternItem::Any) => true,
            (DtnPatternItem::Any, DtnPatternItem::Glob(pattern)) => pattern.as_str() == "**",
            (DtnPatternItem::Exact(lhs1, lhs2), DtnPatternItem::Exact(rhs1, rhs2)) => {
                lhs1 == rhs1 && lhs2 == rhs2
            }
            (DtnPatternItem::Exact(node_name, demux), DtnPatternItem::Glob(pattern)) => {
                do_glob(node_name, demux, pattern)
            }
            (DtnPatternItem::Glob(_lhs), DtnPatternItem::Glob(_rhs)) => {
                // TODO: We just have to say true here, everything else is too hard
                true
            }
            _ => false,
        }
    }

    pub(super) fn try_to_eid(&self) -> Option<Eid> {
        match self {
            DtnPatternItem::None => Some(Eid::Null),
            DtnPatternItem::Exact(node_name, demux) => Some(Eid::Dtn {
                node_name: node_name.clone(),
                demux: demux.clone(),
            }),
            _ => None,
        }
    }

    pub(crate) fn new_glob(pattern: &str) -> Result<Self, Error> {
        Ok(Self::Glob(
            glob::Pattern::new(
                &urlencoding::decode(pattern).map_err(|e| Error::ParseError(e.to_string()))?,
            )
            .map_err(|e| Error::ParseError(e.to_string()))?,
        ))
    }
}

impl std::fmt::Display for DtnPatternItem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::None => write!(f, "none"),
            Self::Any => write!(f, "**"),
            Self::Exact(node_name, demux) => {
                write!(f, "//{}/{demux}", urlencoding::encode(node_name))
            }
            Self::Glob(pattern) => {
                let pattern = urlencoding::encode(pattern.as_str())
                    .replace("%2A", "*")
                    .replace("%2F", "/")
                    .replace("%3F", "?")
                    .replace("%5B", "[")
                    .replace("%5D", "]");
                write!(f, "//{pattern}")
            }
        }
    }
}

// dtn-pat-item = "dtn:" dtn-wkssp-exact / dtn-fullssp
pub(super) fn parse_dtn_pat_item(input: &mut &str) -> ModalResult<EidPatternItem> {
    preceded(
        "dtn:",
        alt((
            "none".map(|_| EidPatternItem::DtnPatternItem(DtnPatternItem::None)),
            "**".map(|_| EidPatternItem::DtnPatternItem(DtnPatternItem::Any)),
            parse_dtn_fullssp.map(EidPatternItem::DtnPatternItem),
        )),
    )
    .parse_next(input)
}

// dtn-fullssp = "//" dtn-exact-ssp / dtn-glob
fn parse_dtn_fullssp(input: &mut &str) -> ModalResult<DtnPatternItem> {
    preceded("//", alt((parse_dtn_exact_ssp, parse_dtn_glob))).parse_next(input)
}

// dtn-exact-ssp = reg-name "/" *VPATH
fn parse_dtn_exact_ssp(input: &mut &str) -> ModalResult<DtnPatternItem> {
    (
        terminated(parse_regname, "/"),
        take_while(0.., ('\x21'..='\x7b', /* No '|' (0x7c) */ '\x7d', '\x7e')),
    )
        .try_map(|(node_name, demux)| {
            if demux.find(['?', '*', '[']).is_some() {
                // Looks like a glob
                DtnPatternItem::new_glob(&format!(
                    "{node_name}/{}",
                    urlencoding::decode(demux).map_err(|e| Error::ParseError(e.to_string()))?
                ))
            } else {
                Ok(DtnPatternItem::Exact(node_name, demux.into()))
            }
        })
        .parse_next(input)
}

fn parse_regname(input: &mut &str) -> ModalResult<Box<str>> {
    take_while(
        0..,
        (
            AsChar::is_alphanum,
            '-',
            '.',
            '_',
            '~',
            '!',
            '$',
            '&',
            '\'',
            '(',
            ')',
            //'*', <-- Force a glob for node_names with *
            '+',
            ',',
            ';',
            '=',
            ('%', AsChar::is_hex_digit, AsChar::is_hex_digit),
        ),
    )
    .try_map(|v| urlencoding::decode(v).map(|s| s.into_owned().into()))
    .parse_next(input)
}

fn parse_dtn_glob(input: &mut &str) -> ModalResult<DtnPatternItem> {
    (
        terminated(
            take_while(
                0..,
                (
                    AsChar::is_alphanum,
                    '-',
                    '.',
                    '_',
                    '~',
                    '!',
                    '$',
                    '&',
                    '\'',
                    '(',
                    ')',
                    '*',
                    '+',
                    ',',
                    ';',
                    '=',
                    '[',
                    '?',
                    ('%', AsChar::is_hex_digit, AsChar::is_hex_digit),
                ),
            ),
            "/",
        ),
        take_while(0.., ('\x21'..='\x7b', /* No '|' (0x7c) */ '\x7d', '\x7e')),
    )
        .try_map(|(node_name, demux)| {
            DtnPatternItem::new_glob(&format!(
                "{}/{}",
                urlencoding::decode(node_name).map_err(|e| Error::ParseError(e.to_string()))?,
                urlencoding::decode(demux).map_err(|e| Error::ParseError(e.to_string()))?
            ))
        })
        .parse_next(input)
}

fn do_glob(node_name: &str, demux: &str, pattern: &glob::Pattern) -> bool {
    pattern.matches_with(
        &format!("{node_name}//{demux}"),
        glob::MatchOptions {
            case_sensitive: false,
            require_literal_separator: true,
            require_literal_leading_dot: false,
        },
    )
}
