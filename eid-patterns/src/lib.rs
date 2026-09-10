#![cfg_attr(not(feature = "std"), no_std)]

/*!
EID pattern matching for BPv7 Endpoint Identifiers.

Provides wildcard and glob-based pattern matching over `ipn` and `dtn` scheme
EIDs as defined in RFC 9171. Patterns can be parsed from text representations
such as `ipn:*.*` or union sets like `ipn:1.1|ipn:2.*`. The
crate supports subset testing, specificity scoring for route selection, and
conversion to/from exact EIDs.

# Feature Flags

- `std`: links the standard library and enables the dependencies' `std` features; without it the crate is `no_std` + `alloc`.
- `dtn-pat-item`: enables `dtn` scheme glob pattern items (implies `std`; adds the `percent-encoding` and `glob` dependencies). Without it, `dtn` EIDs are only matchable by scheme-family wildcards.
- `serde`: `Serialize`/`Deserialize` for [`EidPattern`] through its text form.
*/

extern crate alloc;

use alloc::{
    borrow::Cow,
    boxed::Box,
    string::{String, ToString},
    vec::Vec,
};
use core::{cmp::Ordering, fmt};

use hardy_bpv7::eid::{DtnNodeId, Eid, IpnNodeId, NodeId};
use thiserror::Error;

#[cfg(feature = "dtn-pat-item")]
use crate::dtn_pattern::DtnPatternItem;
use crate::ipn_pattern::IpnPatternItem;

mod ipn_pattern;
mod parse;

#[cfg(feature = "dtn-pat-item")]
mod dtn_pattern;

/// Errors produced by EID pattern parsing and conversion.
#[derive(Error, Debug)]
pub enum Error {
    /// The input string could not be parsed as a valid EID pattern.
    #[error("Parse error: {0}")]
    ParseError(String),

    /// The pattern does not denote exactly one EID: it contains wildcards,
    /// its items name different EIDs, or an exact-looking item denotes no
    /// valid EID (e.g. `ipn:0.0.5`).
    #[error("Not an exact Eid")]
    NotExact,
}

pub type Result<T> = core::result::Result<T, Error>;

/// The private representation of an [`EidPattern`].
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum Repr {
    /// Matches any EID (displayed as `*:**`).
    Any,
    /// A union of one or more pattern items; matches if any item matches.
    Set(Box<[EidPatternItem]>),
}

/// A pattern that matches one or more BPv7 Endpoint Identifiers.
///
/// A pattern is either the catch-all (`*:**`, matching every EID) or a union
/// of one or more scheme-specific items (pipe-separated in text form, e.g.
/// `ipn:1.*|dtn://node/**`). The representation is private: patterns are
/// built by parsing ([`FromStr`](core::str::FromStr)) or converting from an
/// [`Eid`], both of which produce canonical values, so two patterns that
/// match the same EIDs by the same spelling always compare equal.
#[derive(Clone, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(into = "String"))]
#[cfg_attr(feature = "serde", serde(try_from = "Cow<'_,str>"))]
pub struct EidPattern(Repr);

impl fmt::Debug for EidPattern {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // The text form is the canonical representation; debug output shows
        // it rather than the private structure.
        write!(f, "EidPattern({self})")
    }
}

impl PartialOrd for EidPattern {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for EidPattern {
    fn cmp(&self, other: &Self) -> Ordering {
        // Higher specificity score = Less (most specific patterns first in BTreeMap)
        let self_score = self.specificity_score().unwrap_or(0);
        let other_score = other.specificity_score().unwrap_or(0);
        other_score.cmp(&self_score).then_with(|| {
            // Structural tiebreaker for equal scores
            match (&self.0, &other.0) {
                (Repr::Any, Repr::Any) => Ordering::Equal,
                (Repr::Any, Repr::Set(_)) => Ordering::Less,
                (Repr::Set(_), Repr::Any) => Ordering::Greater,
                (Repr::Set(a), Repr::Set(b)) => a.cmp(b),
            }
        })
    }
}

impl EidPattern {
    /// The catch-all pattern (`*:**`).
    fn any() -> Self {
        EidPattern(Repr::Any)
    }

    /// A pattern from a non-empty item set.
    fn from_items(items: Box<[EidPatternItem]>) -> Self {
        EidPattern(Repr::Set(items))
    }

    /// Returns `true` if the pattern matches the given EID.
    #[inline]
    pub fn matches(&self, eid: &Eid) -> bool {
        match &self.0 {
            Repr::Any => true,
            Repr::Set(items) => items.iter().any(|i| i.matches(eid)),
        }
    }

    /// Decomposes the pattern into its atomic route keys: a multi-item union
    /// yields one single-item pattern per member, and any other pattern
    /// yields itself. Route selection compares the specificity of the pattern
    /// that matched, so a union must never be stored as one key: an aggregate
    /// score would let a broad member drag a specific sibling behind routes
    /// the sibling strictly beats. A union route is shorthand for one route
    /// per member.
    pub fn into_atoms(self) -> impl Iterator<Item = EidPattern> {
        let atoms: Vec<EidPattern> = match self.0 {
            Repr::Set(items) if items.len() > 1 => items
                .into_vec()
                .into_iter()
                .map(|item| EidPattern::from_items([item].into()))
                .collect(),
            repr => Vec::from([EidPattern(repr)]),
        };
        atoms.into_iter()
    }

    /// Harmonized Specificity Score.
    ///
    /// A union set scores as its *broadest* (least specific) member, since it
    /// matches the union of its members and is therefore only as narrow as the
    /// widest one. Returns `None` for an empty set or if any member is
    /// unscoreable.
    pub fn specificity_score(&self) -> Option<u32> {
        match &self.0 {
            Repr::Any => Some(0),
            Repr::Set(items) => items.iter().map(|i| i.specificity_score()).min().flatten(),
        }
    }

    /// If any item in this pattern is a LocalNode pattern (the `ipn:!.*` sentinel),
    /// return a new pattern with the sentinel replaced by the concrete `node_id`.
    /// Returns `None` if no LocalNode pattern was found.
    pub fn expand_local_node(&self, node_id: &IpnNodeId) -> Option<Self> {
        match &self.0 {
            Repr::Any => None,
            Repr::Set(items) => {
                let mut expanded = Vec::new();
                let mut changed = false;
                for item in items.iter() {
                    if let Some(new_item) = item.expand_local_node(node_id) {
                        expanded.push(new_item);
                        changed = true;
                    } else {
                        expanded.push(item.clone());
                    }
                }
                changed.then(|| EidPattern::from_items(expanded.into()))
            }
        }
    }

    /// Returns `true` if `self` is a subset of (or equal to) `other`.
    pub fn is_subset(&self, other: &Self) -> bool {
        match (&self.0, &other.0) {
            (_, Repr::Any) => true,
            (Repr::Any, _) => false,
            (Repr::Set(lhs), Repr::Set(rhs)) => {
                // Every member of lhs must be a subset of at least one member in rhs
                lhs.iter().all(|l| rhs.iter().any(|r| l.is_subset(r)))
            }
        }
    }
}

impl TryFrom<Cow<'_, str>> for EidPattern {
    type Error = Error;

    fn try_from(value: Cow<'_, str>) -> Result<Self> {
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
        EidPattern::from_items(
            [EidPatternItem::IpnPatternItem(IpnPatternItem::new(
                value.allocator_id,
                value.node_number,
                None,
            ))]
            .into(),
        )
    }
}

impl From<DtnNodeId> for EidPattern {
    #[cfg(feature = "dtn-pat-item")]
    fn from(value: DtnNodeId) -> Self {
        EidPattern::from_items(
            [EidPatternItem::DtnPatternItem(
                DtnPatternItem::new_glob(format!("{}/**", value.node_name).as_str())
                    .expect("dtn node names contain no glob metacharacters"),
            )]
            .into(),
        )
    }

    #[cfg(not(feature = "dtn-pat-item"))]
    fn from(_: DtnNodeId) -> Self {
        EidPattern::from_items(
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
            NodeId::LocalNode => EidPattern::from_items(
                [EidPatternItem::IpnPatternItem(IpnPatternItem::new(
                    0,
                    u32::MAX,
                    None,
                ))]
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
            Eid::Null => EidPattern::from_items(
                [
                    EidPatternItem::IpnPatternItem(IpnPatternItem::new(0, 0, Some(0))),
                    #[cfg(feature = "dtn-pat-item")]
                    EidPatternItem::DtnPatternItem(DtnPatternItem::None),
                ]
                .into(),
            ),
            Eid::LocalNode(service_number) => EidPattern::from_items(
                [EidPatternItem::IpnPatternItem(IpnPatternItem::new(
                    0,
                    u32::MAX,
                    Some(service_number),
                ))]
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
            } => EidPattern::from_items(
                [EidPatternItem::IpnPatternItem(IpnPatternItem::new(
                    allocator_id,
                    node_number,
                    Some(service_number),
                ))]
                .into(),
            ),
            #[cfg(feature = "dtn-pat-item")]
            Eid::Dtn {
                node_name,
                service_name,
            } => EidPattern::from_items(
                [EidPatternItem::DtnPatternItem(DtnPatternItem::Exact(
                    node_name.node_name,
                    service_name,
                ))]
                .into(),
            ),
            #[cfg(not(feature = "dtn-pat-item"))]
            Eid::Dtn { .. } => EidPattern::from_items(
                [
                    EidPatternItem::AnyNumericScheme(1),
                    EidPatternItem::AnyTextScheme("dtn".into()),
                ]
                .into(),
            ),
            Eid::Unknown { scheme, .. } => {
                EidPattern::from_items([EidPatternItem::AnyNumericScheme(scheme)].into())
            }
        }
    }
}

impl TryFrom<EidPattern> for Eid {
    type Error = Error;

    /// Succeeds when every item in the set denotes the same single EID. This
    /// covers the usual one-item exact pattern, and also the two-item set that
    /// `From<Eid>` produces for [`Eid::Null`] (`ipn:0.0` | `dtn:none`), whose
    /// items both name the null endpoint.
    fn try_from(value: EidPattern) -> Result<Self> {
        match value.0 {
            Repr::Set(items) => {
                let mut items = items.iter();
                let first = items
                    .next()
                    .and_then(EidPatternItem::try_to_eid)
                    .ok_or(Error::NotExact)?;
                for item in items {
                    if item.try_to_eid().as_ref() != Some(&first) {
                        return Err(Error::NotExact);
                    }
                }
                Ok(first)
            }
            Repr::Any => Err(Error::NotExact),
        }
    }
}

impl fmt::Display for EidPattern {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.0 {
            Repr::Any => write!(f, "*:**"),
            Repr::Set(items) => {
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

/// The canonical numeric scheme code of an EID (`dtn` = 1, `ipn` = 2), or the
/// raw code for an unrecognised scheme. The null endpoint is scheme-ambiguous
/// (`dtn:none` / `ipn:0.0`) and is matched by the concrete ipn/dtn pattern
/// arms rather than by scheme-family wildcards, so it maps to `None` here.
fn eid_numeric_scheme(eid: &Eid) -> Option<u64> {
    match eid {
        Eid::Null => None,
        Eid::LocalNode(_) | Eid::LegacyIpn { .. } | Eid::Ipn { .. } => Some(2),
        Eid::Dtn { .. } => Some(1),
        Eid::Unknown { scheme, .. } => Some(*scheme),
    }
}

/// The numeric code for a text scheme name, for the schemes that have both forms.
fn numeric_scheme_of_text(scheme: &str) -> Option<u64> {
    match scheme {
        "dtn" => Some(1),
        "ipn" => Some(2),
        _ => None,
    }
}

/// A single scheme-specific EID pattern within an [`EidPattern`] union set.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
enum EidPatternItem {
    /// Matches any EID using the given numeric scheme code (e.g. `2:**`).
    AnyNumericScheme(u64),
    /// Matches any EID using the given text scheme name (e.g. `dtn:**`).
    AnyTextScheme(String),
    /// A pattern over the `ipn` scheme with optional wildcards on each component.
    IpnPatternItem(IpnPatternItem),
    /// A pattern over the `dtn` scheme using glob-style matching.
    #[cfg(feature = "dtn-pat-item")]
    DtnPatternItem(DtnPatternItem),
}

impl EidPatternItem {
    #[inline]
    fn matches(&self, eid: &Eid) -> bool {
        match self {
            EidPatternItem::IpnPatternItem(i) => i.matches(eid),
            #[cfg(feature = "dtn-pat-item")]
            EidPatternItem::DtnPatternItem(i) => i.matches(eid),
            // Scheme-family wildcards (`N:**` / `scheme:**`) match any EID of
            // that scheme — including an `Eid::Unknown` decoded from the wire.
            EidPatternItem::AnyNumericScheme(n) => eid_numeric_scheme(eid) == Some(*n),
            // A text scheme with no numeric code (anything but `dtn`/`ipn`)
            // matches nothing: guard on `numeric_scheme_of_text` so an unknown
            // scheme does not fall through to `None == None` and wrongly match
            // the null endpoint (whose `eid_numeric_scheme` is also `None`).
            EidPatternItem::AnyTextScheme(s) => {
                numeric_scheme_of_text(s).is_some_and(|n| eid_numeric_scheme(eid) == Some(n))
            }
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

    fn expand_local_node(&self, node_id: &IpnNodeId) -> Option<Self> {
        match self {
            EidPatternItem::IpnPatternItem(i) => i
                .expand_local_node(node_id)
                .map(EidPatternItem::IpnPatternItem),
            _ => None,
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
    fn specificity_score(&self) -> Option<u32> {
        match self {
            EidPatternItem::IpnPatternItem(i) => i.specificity_score(),
            #[cfg(feature = "dtn-pat-item")]
            EidPatternItem::DtnPatternItem(i) => i.specificity_score(),
            // Scheme-level wildcards score 0 (equivalent to ipn:** / dtn:**)
            EidPatternItem::AnyNumericScheme(_) | EidPatternItem::AnyTextScheme(_) => Some(0),
        }
    }
}

impl fmt::Display for EidPatternItem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EidPatternItem::IpnPatternItem(i) => write!(f, "ipn:{i}"),
            #[cfg(feature = "dtn-pat-item")]
            EidPatternItem::DtnPatternItem(i) => write!(f, "dtn:{i}"),
            EidPatternItem::AnyNumericScheme(v) => write!(f, "{v}:**"),
            EidPatternItem::AnyTextScheme(v) => write!(f, "{v}:**"),
        }
    }
}
