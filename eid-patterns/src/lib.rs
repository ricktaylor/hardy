#![cfg_attr(not(feature = "std"), no_std)]

//! EID pattern matching for BPv7 Endpoint Identifiers.
//!
//! Provides wildcard and glob-based pattern matching over `ipn` and `dtn` scheme
//! EIDs as defined in RFC 9171. Patterns can be parsed from text representations
//! such as `ipn:*.*`, `dtn://**`, or union sets like `ipn:1.1|ipn:2.*`. The
//! crate supports subset testing, specificity scoring for route selection, and
//! conversion to/from exact EIDs.

extern crate alloc;

use alloc::{
    borrow::Cow,
    boxed::Box,
    string::{String, ToString},
    vec,
    vec::Vec,
};

use hardy_bpv7::eid::{DtnNodeId, Eid, IpnNodeId, NodeId};
use thiserror::Error;

mod ipn_pattern;
mod parse;

#[cfg(feature = "dtn-pat-item")]
mod dtn_pattern;

#[cfg(test)]
mod str_tests;

/// Errors produced by EID pattern parsing and conversion.
#[derive(Error, Debug)]
pub enum Error {
    /// The input string could not be parsed as a valid EID pattern.
    #[error("Parse error: {0}")]
    ParseError(String),

    /// The pattern is not an exact EID (contains wildcards or multiple items).
    #[error("Not an exact Eid")]
    NotExact,
}

/// A pattern that matches one or more BPv7 Endpoint Identifiers.
///
/// `Any` matches every EID. `Set` holds one or more [`EidPatternItem`]s joined
/// as a union (pipe-separated in text form, e.g. `ipn:1.*|dtn://node/**`).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(into = "String"))]
#[cfg_attr(feature = "serde", serde(try_from = "Cow<'_,str>"))]
pub enum EidPattern {
    /// Matches any EID (displayed as `*:**`).
    Any,
    /// A union of one or more pattern items; matches if any item matches.
    Set(Box<[EidPatternItem]>),
}

impl PartialOrd for EidPattern {
    fn partial_cmp(&self, other: &Self) -> Option<core::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for EidPattern {
    fn cmp(&self, other: &Self) -> core::cmp::Ordering {
        // Higher specificity score = Less (most specific patterns first in BTreeMap)
        let self_score = self.specificity_score().unwrap_or(0);
        let other_score = other.specificity_score().unwrap_or(0);
        other_score.cmp(&self_score).then_with(|| {
            // Structural tiebreaker for equal scores
            match (self, other) {
                (EidPattern::Any, EidPattern::Any) => core::cmp::Ordering::Equal,
                (EidPattern::Any, EidPattern::Set(_)) => core::cmp::Ordering::Less,
                (EidPattern::Set(_), EidPattern::Any) => core::cmp::Ordering::Greater,
                (EidPattern::Set(a), EidPattern::Set(b)) => a.cmp(b),
            }
        })
    }
}

impl EidPattern {
    /// Returns `true` if the pattern matches the given EID.
    #[inline]
    pub fn matches(&self, eid: &Eid) -> bool {
        match self {
            EidPattern::Any => true,
            EidPattern::Set(items) => items.iter().any(|i| i.matches(eid)),
        }
    }

    /// Harmonized Specificity Score.
    ///
    /// Returns `None` for union sets (multiple items) or patterns violating
    /// monotonic constraints.
    pub fn specificity_score(&self) -> Option<u32> {
        match self {
            EidPattern::Any => Some(0),
            EidPattern::Set(items) if items.len() == 1 => items[0].specificity_score(),
            EidPattern::Set(_) => None, // Union sets not valid for scoring
        }
    }

    /// Returns `true` if `self` is a subset of (or equal to) `other`.
    pub fn is_subset(&self, other: &Self) -> bool {
        match (self, other) {
            (_, EidPattern::Any) => true,
            (EidPattern::Any, _) => false,
            (EidPattern::Set(lhs), EidPattern::Set(rhs)) => {
                // Every member of lhs must be a subset of at least one member in rhs
                lhs.iter().all(|l| rhs.iter().any(|r| l.is_subset(r)))
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

impl From<IpnNodeId> for EidPattern {
    fn from(value: IpnNodeId) -> Self {
        EidPattern::Set(
            [EidPatternItem::IpnPatternItem(
                ipn_pattern::IpnPatternItem::new(value.allocator_id, value.node_number, None),
            )]
            .into(),
        )
    }
}

impl From<DtnNodeId> for EidPattern {
    #[cfg(feature = "dtn-pat-item")]
    fn from(value: DtnNodeId) -> Self {
        EidPattern::Set(
            [EidPatternItem::DtnPatternItem(
                dtn_pattern::DtnPatternItem::new_glob(format!("{}/**", value.node_name).as_str())
                    .expect("Invalid glob"),
            )]
            .into(),
        )
    }

    #[cfg(not(feature = "dtn-pat-item"))]
    fn from(_: DtnNodeId) -> Self {
        EidPattern::Set(
            [
                EidPatternItem::AnyNumericScheme(1),
                EidPatternItem::AnyTextScheme("dtn".into()),
            ]
            .into(),
        )
    }
}

impl From<NodeId> for EidPattern {
    fn from(value: NodeId) -> Self {
        match value {
            NodeId::LocalNode => EidPattern::Set(
                [EidPatternItem::IpnPatternItem(
                    ipn_pattern::IpnPatternItem::new(0, u32::MAX, None),
                )]
                .into(),
            ),
            NodeId::Ipn(node_id) => node_id.into(),
            NodeId::Dtn(node_id) => node_id.into(),
        }
    }
}

impl From<Eid> for EidPattern {
    fn from(value: Eid) -> Self {
        match value {
            Eid::Null => EidPattern::Set(
                [
                    EidPatternItem::IpnPatternItem(ipn_pattern::IpnPatternItem::new(0, 0, Some(0))),
                    #[cfg(feature = "dtn-pat-item")]
                    EidPatternItem::DtnPatternItem(dtn_pattern::DtnPatternItem::None),
                ]
                .into(),
            ),
            Eid::LocalNode(service_number) => EidPattern::Set(
                [EidPatternItem::IpnPatternItem(
                    ipn_pattern::IpnPatternItem::new(0, u32::MAX, Some(service_number)),
                )]
                .into(),
            ),
            Eid::LegacyIpn {
                fqnn:
                    IpnNodeId {
                        allocator_id,
                        node_number,
                    },
                service_number,
            }
            | Eid::Ipn {
                fqnn:
                    IpnNodeId {
                        allocator_id,
                        node_number,
                    },
                service_number,
            } => EidPattern::Set(
                [EidPatternItem::IpnPatternItem(
                    ipn_pattern::IpnPatternItem::new(
                        allocator_id,
                        node_number,
                        Some(service_number),
                    ),
                )]
                .into(),
            ),
            #[cfg(feature = "dtn-pat-item")]
            Eid::Dtn {
                node_name,
                service_name,
            } => EidPattern::Set(
                [EidPatternItem::DtnPatternItem(
                    dtn_pattern::DtnPatternItem::Exact(node_name.node_name, service_name),
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

impl core::fmt::Display for EidPattern {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
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

/// A single scheme-specific EID pattern within an [`EidPattern`] union set.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum EidPatternItem {
    /// Matches any EID using the given numeric scheme code (e.g. `2:**`).
    AnyNumericScheme(u64),
    /// Matches any EID using the given text scheme name (e.g. `dtn:**`).
    AnyTextScheme(String),
    /// A pattern over the `ipn` scheme with optional wildcards on each component.
    IpnPatternItem(ipn_pattern::IpnPatternItem),
    /// A pattern over the `dtn` scheme using glob-style matching.
    #[cfg(feature = "dtn-pat-item")]
    DtnPatternItem(dtn_pattern::DtnPatternItem),
}

impl EidPatternItem {
    #[inline]
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

    /// Harmonized Specificity Score.
    ///
    /// Returns `None` if the pattern violates monotonic constraints.
    pub fn specificity_score(&self) -> Option<u32> {
        match self {
            EidPatternItem::IpnPatternItem(i) => i.specificity_score(),
            #[cfg(feature = "dtn-pat-item")]
            EidPatternItem::DtnPatternItem(i) => i.specificity_score(),
            // Scheme-level wildcards score 0 (equivalent to ipn:** / dtn:**)
            EidPatternItem::AnyNumericScheme(_) | EidPatternItem::AnyTextScheme(_) => Some(0),
        }
    }
}

impl core::fmt::Display for EidPatternItem {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            EidPatternItem::IpnPatternItem(i) => write!(f, "ipn:{i}"),
            #[cfg(feature = "dtn-pat-item")]
            EidPatternItem::DtnPatternItem(i) => write!(f, "dtn:{i}"),
            EidPatternItem::AnyNumericScheme(v) => write!(f, "{v}:**"),
            EidPatternItem::AnyTextScheme(v) => write!(f, "{v}:**"),
        }
    }
}
