use super::*;
use winnow::{
    ModalResult, Parser,
    combinator::{alt, preceded, terminated},
    stream::AsChar,
    token::take_while,
};

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum DtnPatternItem {
    None,
    All,
    Exact(Box<str>, Box<str>),
    Glob(glob::Pattern),
}

impl DtnPatternItem {
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
            glob::Pattern::new(pattern).map_err(|e| Error::ParseError(e.to_string()))?,
        ))
    }
}

impl std::fmt::Display for DtnPatternItem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::None => write!(f, "none"),
            Self::All => write!(f, "**"),
            Self::Exact(node_name, demux) => write!(f, "//{node_name}/{demux}"),
            Self::Glob(pattern) => write!(f, "//{pattern}"),
        }
    }
}

// dtn-pat-item = "dtn:" dtn-wkssp-exact / dtn-fullssp
pub(super) fn parse_dtn_pat_item(input: &mut &str) -> ModalResult<EidPatternItem> {
    preceded(
        "dtn:",
        alt((
            "none".map(|_| EidPatternItem::DtnPatternItem(DtnPatternItem::None)),
            "**".map(|_| EidPatternItem::DtnPatternItem(DtnPatternItem::All)),
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
                DtnPatternItem::new_glob(&format!("{node_name}/{demux}"))
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
    take_while(1.., ('\x21'..='\x7b', /* No '|' (0x7c) */ '\x7d', '\x7e'))
        .try_map(DtnPatternItem::new_glob)
        .parse_next(input)
}
