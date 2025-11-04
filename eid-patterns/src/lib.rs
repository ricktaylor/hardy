use hardy_bpv7::eid::Eid;
use std::borrow::Cow;
use thiserror::Error;

mod ipn_pattern;
mod parse;

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

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(into = "String"))]
#[cfg_attr(feature = "serde", serde(try_from = "Cow<'_,str>"))]
pub enum EidPattern {
    Any,
    Set(Box<[EidPatternItem]>),
}

impl EidPattern {
    pub fn matches(&self, eid: &Eid) -> bool {
        match self {
            EidPattern::Any => true,
            EidPattern::Set(items) => items.iter().any(|i| i.matches(eid)),
        }
    }

    /// Is `self`` a subset (or equal to) `other`
    pub fn is_subset(&self, other: &Self) -> bool {
        match (self, other) {
            (_, EidPattern::Any) => true,
            (EidPattern::Any, _) => false,
            (EidPattern::Set(lhs), EidPattern::Set(rhs)) => {
                // Every member must be a subset of at least one member in rhs
                !lhs.iter().any(|l| rhs.iter().any(|r| !l.is_subset(r)))
            }
        }
    }
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
                for (i, p) in items.iter().enumerate() {
                    if i != 0 {
                        write!(f, "|")?;
                    }
                    write!(f, "{p}")?;
                }
                Ok(())
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum EidPatternItem {
    AnyNumericScheme(u64),
    AnyTextScheme(String),
    IpnPatternItem(ipn_pattern::IpnPatternItem),
    #[cfg(feature = "dtn-pat-item")]
    DtnPatternItem(dtn_pattern::DtnPatternItem),
}

impl EidPatternItem {
    fn matches(&self, eid: &Eid) -> bool {
        match self {
            EidPatternItem::IpnPatternItem(i) => i.matches(eid),
            #[cfg(feature = "dtn-pat-item")]
            EidPatternItem::DtnPatternItem(i) => i.matches(eid),
            _ => false,
        }
    }

    fn is_subset(&self, other: &Self) -> bool {
        match (self, other) {
            (EidPatternItem::AnyNumericScheme(lhs), EidPatternItem::AnyNumericScheme(rhs)) => {
                lhs == rhs
            }
            (EidPatternItem::AnyNumericScheme(s_n), EidPatternItem::AnyTextScheme(s_str))
            | (EidPatternItem::AnyTextScheme(s_str), EidPatternItem::AnyNumericScheme(s_n)) => {
                (*s_n == 1 && s_str == "dtn") || (*s_n == 2 && s_str == "ipn")
            }
            (EidPatternItem::AnyTextScheme(lhs), EidPatternItem::AnyTextScheme(rhs)) => lhs == rhs,
            (EidPatternItem::IpnPatternItem(_), EidPatternItem::AnyNumericScheme(2)) => true,
            (EidPatternItem::IpnPatternItem(_), EidPatternItem::AnyTextScheme(s)) => s == "ipn",
            (EidPatternItem::IpnPatternItem(lhs), EidPatternItem::IpnPatternItem(rhs)) => {
                lhs.is_subset(rhs)
            }
            #[cfg(feature = "dtn-pat-item")]
            (EidPatternItem::IpnPatternItem(lhs), EidPatternItem::DtnPatternItem(rhs)) => {
                lhs.try_to_eid() == Some(Eid::Null) && rhs.try_to_eid() == Some(Eid::Null)
            }
            #[cfg(feature = "dtn-pat-item")]
            (EidPatternItem::DtnPatternItem(_), EidPatternItem::AnyNumericScheme(1)) => true,
            #[cfg(feature = "dtn-pat-item")]
            (EidPatternItem::DtnPatternItem(_), EidPatternItem::AnyTextScheme(s)) => s == "dtn",
            #[cfg(feature = "dtn-pat-item")]
            (EidPatternItem::DtnPatternItem(lhs), EidPatternItem::IpnPatternItem(rhs)) => {
                lhs.try_to_eid() == Some(Eid::Null) && rhs.try_to_eid() == Some(Eid::Null)
            }
            #[cfg(feature = "dtn-pat-item")]
            (EidPatternItem::DtnPatternItem(lhs), EidPatternItem::DtnPatternItem(rhs)) => {
                lhs.is_subset(rhs)
            }
            _ => false,
        }
    }

    fn try_to_eid(&self) -> Option<Eid> {
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
