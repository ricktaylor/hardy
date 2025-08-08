use hardy_bpv7::eid::Eid;
use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use thiserror::Error;

mod ipn_pattern;
mod parse;

mod eid_pattern_map;

#[cfg(feature = "dtn-pat-item")]
mod dtn_pattern;

#[cfg(test)]
mod str_tests;

#[derive(Error, Debug)]
pub enum Error {
    #[error("Parse error: {0}")]
    ParseError(String),

    #[error("Not an exact Eid")]
    NotExact,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(into = "String")]
#[serde(try_from = "Cow<'_,str>")]
pub enum EidPattern {
    Set(Box<[EidPatternItem]>),
    Any,
}

impl TryFrom<Cow<'_, str>> for EidPattern {
    type Error = Error;

    fn try_from(value: Cow<'_, str>) -> Result<Self, Self::Error> {
        value.parse()
    }
}

impl From<EidPattern> for String {
    fn from(value: EidPattern) -> Self {
        value.to_string()
    }
}

impl From<Eid> for EidPattern {
    fn from(value: Eid) -> Self {
        match value {
            Eid::Null => EidPattern::Set(
                [
                    EidPatternItem::IpnPatternItem(ipn_pattern::IpnPatternItem::new(0, 0, 0)),
                    #[cfg(feature = "dtn-pat-item")]
                    EidPatternItem::DtnPatternItem(dtn_pattern::DtnPatternItem::None),
                ]
                .into(),
            ),
            Eid::LocalNode { service_number } => EidPattern::Set(
                [EidPatternItem::IpnPatternItem(
                    ipn_pattern::IpnPatternItem::new(0, u32::MAX, service_number),
                )]
                .into(),
            ),
            Eid::LegacyIpn {
                allocator_id,
                node_number,
                service_number,
            }
            | Eid::Ipn {
                allocator_id,
                node_number,
                service_number,
            } => EidPattern::Set(
                [EidPatternItem::IpnPatternItem(
                    ipn_pattern::IpnPatternItem::new(allocator_id, node_number, service_number),
                )]
                .into(),
            ),
            #[cfg(feature = "dtn-pat-item")]
            Eid::Dtn { node_name, demux } => EidPattern::Set(
                [EidPatternItem::DtnPatternItem(
                    dtn_pattern::DtnPatternItem::Exact(node_name, demux),
                )]
                .into(),
            ),
            #[cfg(not(feature = "dtn-pat-item"))]
            Eid::Dtn { .. } => EidPattern::Set(
                [
                    EidPatternItem::AnyNumericScheme(1),
                    EidPatternItem::AnyTextScheme("dtn".into()),
                ]
                .into(),
            ),

            Eid::Unknown { scheme, .. } => {
                EidPattern::Set([EidPatternItem::AnyNumericScheme(scheme)].into())
            }
        }
    }
}

impl TryFrom<EidPattern> for Eid {
    type Error = Error;

    fn try_from(value: EidPattern) -> Result<Self, Self::Error> {
        match value {
            EidPattern::Set(items) if items.len() == 1 => {
                items[0].try_to_eid().ok_or(Error::NotExact)
            }
            _ => Err(Error::NotExact),
        }
    }
}

impl std::fmt::Display for EidPattern {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EidPattern::Any => write!(f, "*:**"),
            EidPattern::Set(items) => {
                let mut first = true;
                for i in items {
                    if first {
                        first = false;
                    } else {
                        write!(f, "|")?;
                    }
                    write!(f, "{i}")?;
                }
                Ok(())
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum EidPatternItem {
    IpnPatternItem(ipn_pattern::IpnPatternItem),
    #[cfg(feature = "dtn-pat-item")]
    DtnPatternItem(dtn_pattern::DtnPatternItem),
    AnyNumericScheme(u64),
    AnyTextScheme(String),
}

impl EidPatternItem {
    pub(crate) fn try_to_eid(&self) -> Option<Eid> {
        match self {
            EidPatternItem::IpnPatternItem(i) => i.try_to_eid(),
            #[cfg(feature = "dtn-pat-item")]
            EidPatternItem::DtnPatternItem(i) => i.try_to_eid(),
            _ => None,
        }
    }
}

impl std::fmt::Display for EidPatternItem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EidPatternItem::IpnPatternItem(i) => write!(f, "ipn:{i}"),
            #[cfg(feature = "dtn-pat-item")]
            EidPatternItem::DtnPatternItem(i) => write!(f, "dtn:{i}"),
            EidPatternItem::AnyNumericScheme(v) => write!(f, "{v}:**"),
            EidPatternItem::AnyTextScheme(v) => write!(f, "{v}:**"),
        }
    }
}

pub use eid_pattern_map::{EidPatternMap, EidPatternSet};
