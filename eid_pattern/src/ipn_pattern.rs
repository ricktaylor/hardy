use super::*;
use core::ops::RangeInclusive;
use winnow::{
    ModalResult, Parser,
    ascii::dec_uint,
    combinator::{alt, delimited, opt, preceded, separated},
};

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct IpnPatternItem {
    pub(crate) allocator_id: IpnPattern,
    pub(crate) node_number: IpnPattern,
    pub(crate) service_number: IpnPattern,
}

pub const ANY: IpnPatternItem = IpnPatternItem {
    allocator_id: IpnPattern::Wildcard,
    node_number: IpnPattern::Wildcard,
    service_number: IpnPattern::Wildcard,
};

impl IpnPatternItem {
    pub(crate) fn new(allocator_id: u32, node_number: u32, service_number: u32) -> Self {
        Self {
            allocator_id: ipn_pattern::IpnPattern::Range(vec![ipn_pattern::IpnInterval::Number(
                allocator_id,
            )]),
            node_number: ipn_pattern::IpnPattern::Range(vec![ipn_pattern::IpnInterval::Number(
                node_number,
            )]),
            service_number: ipn_pattern::IpnPattern::Range(vec![ipn_pattern::IpnInterval::Number(
                service_number,
            )]),
        }
    }

    pub(super) fn matches(&self, eid: &Eid) -> bool {
        match eid {
            Eid::Null => {
                self.allocator_id.matches(0)
                    && self.node_number.matches(0)
                    && self.service_number.matches(0)
            }
            Eid::LocalNode { service_number } => {
                self.allocator_id.matches(0)
                    && self.node_number.matches(u32::MAX)
                    && self.service_number.matches(*service_number)
            }
            Eid::LegacyIpn {
                allocator_id,
                node_number,
                service_number,
            }
            | Eid::Ipn {
                allocator_id,
                node_number,
                service_number,
            } => {
                self.allocator_id.matches(*allocator_id)
                    && self.node_number.matches(*node_number)
                    && self.service_number.matches(*service_number)
            }
            _ => false,
        }
    }

    pub(super) fn is_subset(&self, other: &Self) -> bool {
        self.allocator_id.is_subset(&other.allocator_id)
            && self.node_number.is_subset(&other.node_number)
            && self.service_number.is_subset(&other.service_number)
    }

    pub(super) fn try_to_eid(&self) -> Option<Eid> {
        Some(Eid::Ipn {
            allocator_id: self.allocator_id.try_to_eid()?,
            node_number: self.node_number.try_to_eid()?,
            service_number: self.service_number.try_to_eid()?,
        })
    }
}

impl std::fmt::Display for IpnPatternItem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self == &ANY {
            write!(f, "**")
        } else if self.allocator_id == IpnPattern::Range(vec![IpnInterval::Number(0)]) {
            // Old style without allocator ID
            write!(f, "{}.{}", self.node_number, self.service_number)
        } else {
            // New style with allocator ID
            write!(
                f,
                "{}.{}.{}",
                self.allocator_id, self.node_number, self.service_number
            )
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum IpnPattern {
    Wildcard,
    Range(Vec<IpnInterval>),
}

impl IpnPattern {
    fn matches(&self, v: u32) -> bool {
        let IpnPattern::Range(r) = self else {
            return true;
        };
        r.iter().any(|r| r.matches(v))
    }

    fn is_subset(&self, other: &Self) -> bool {
        match (self, other) {
            (_, IpnPattern::Wildcard) => true,
            (IpnPattern::Wildcard, IpnPattern::Range(_)) => false,
            (IpnPattern::Range(lhs), IpnPattern::Range(rhs)) => {
                // Every member must be a subset of at least one member in rhs
                !lhs.iter().any(|l| rhs.iter().any(|r| !l.is_subset(r)))
            }
        }
    }

    fn try_to_eid(&self) -> Option<u32> {
        match self {
            IpnPattern::Range(r) if r.len() == 1 => r[0].try_to_u32(),
            _ => None,
        }
    }
}

impl std::fmt::Display for IpnPattern {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            IpnPattern::Range(r) if r.len() == 1 => match r[0] {
                IpnInterval::Number(n) => write!(f, "{n}"),
                IpnInterval::Range(_) => write!(f, "[{}]", r[0]),
            },
            IpnPattern::Range(r) => {
                write!(f, "[")?;
                for (i, r) in r.iter().enumerate() {
                    if i > 0 {
                        write!(f, ",")?;
                    }
                    write!(f, "{r}")?;
                }
                write!(f, "]")
            }
            IpnPattern::Wildcard => write!(f, "*"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum IpnInterval {
    Number(u32),
    Range(RangeInclusive<u32>),
}

impl std::fmt::Display for IpnInterval {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            IpnInterval::Number(n) => write!(f, "{n}"),
            IpnInterval::Range(r) if r.end() == &u32::MAX => write!(f, "{}+", r.start()),
            IpnInterval::Range(r) => write!(f, "{}-{}", r.start(), r.end()),
        }
    }
}

impl PartialOrd for IpnInterval {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for IpnInterval {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        match (self, other) {
            (IpnInterval::Number(lhs), IpnInterval::Number(rhs)) => lhs.cmp(rhs),
            (IpnInterval::Number(lhs), IpnInterval::Range(rhs)) => lhs
                .cmp(rhs.start())
                .then_with(|| 0.cmp(&(rhs.end() - rhs.start()))),
            (IpnInterval::Range(lhs), IpnInterval::Number(rhs)) => lhs
                .start()
                .cmp(rhs)
                .then_with(|| (lhs.end() - lhs.start()).cmp(&0)),
            (IpnInterval::Range(lhs), IpnInterval::Range(rhs)) => lhs
                .start()
                .cmp(rhs.start())
                .then_with(|| (lhs.end() - lhs.start()).cmp(&(rhs.end() - rhs.start()))),
        }
    }
}

impl IpnInterval {
    fn matches(&self, v: u32) -> bool {
        match self {
            IpnInterval::Number(n) => *n == v,
            IpnInterval::Range(r) => r.contains(&v),
        }
    }

    fn is_subset(&self, other: &Self) -> bool {
        match (self, other) {
            (IpnInterval::Number(lhs), IpnInterval::Number(rhs)) => lhs == rhs,
            (IpnInterval::Number(lhs), IpnInterval::Range(rhs)) => rhs.contains(lhs),
            (IpnInterval::Range(_), IpnInterval::Number(_)) => false,
            (IpnInterval::Range(lhs), IpnInterval::Range(rhs)) => {
                rhs.start() <= lhs.start() && rhs.end() >= lhs.end()
            }
        }
    }

    fn try_to_u32(&self) -> Option<u32> {
        match self {
            IpnInterval::Number(n) => Some(*n),
            IpnInterval::Range(_) => None,
        }
    }
}

// ipn-pat-item = "ipn:" (ipn-ssp3 / ipn-ssp2)
// ipn-ssp3 = ipn-part-pat nbr-delim ipn-part-pat nbr-delim ipn-part-pat
// OLD: ipn-ssp2 = ipn-part-pat nbr-delim ipn-part-pat
// ipn-ssp2 = ("!" / ipn-part-pat) nbr-delim ipn-part-pat
pub(crate) fn parse_ipn_pat_item(input: &mut &str) -> ModalResult<EidPatternItem> {
    preceded(
        "ipn:",
        alt((
            "**".map(|_| ipn_pattern::ANY),
            preceded("!.", parse_ipn_part_pat).map(|c| IpnPatternItem {
                allocator_id: IpnPattern::Range(vec![IpnInterval::Number(0)]),
                node_number: IpnPattern::Range(vec![IpnInterval::Number(u32::MAX)]),
                service_number: c,
            }),
            (
                parse_ipn_part_pat,
                preceded(".", parse_ipn_part_pat),
                opt(preceded(".", parse_ipn_part_pat)),
            )
                .map(|(a, b, c)| {
                    let (a, b, c) = if let Some(c) = c {
                        (a, b, c)
                    } else {
                        (IpnPattern::Range(vec![IpnInterval::Number(0)]), a, b)
                    };
                    IpnPatternItem {
                        allocator_id: a,
                        node_number: b,
                        service_number: c,
                    }
                }),
        )),
    )
    .map(EidPatternItem::IpnPatternItem)
    .parse_next(input)
}

// ipn-part-pat = ipn-decimal / ipn-range / wildcard
fn parse_ipn_part_pat(input: &mut &str) -> ModalResult<IpnPattern> {
    alt((
        "*".map(|_| IpnPattern::Wildcard),
        dec_uint.map(|v| IpnPattern::Range(vec![IpnInterval::Number(v)])),
        parse_ipn_range,
    ))
    .parse_next(input)
}

// ipn-range = "[" ipn-interval *( "," ipn-interval ) "]"
fn parse_ipn_range(input: &mut &str) -> ModalResult<IpnPattern> {
    delimited("[", separated(1.., parse_ipn_interval, ","), "]")
        .map(|mut intervals: Vec<RangeInclusive<u32>>| {
            if intervals.is_empty() {
                IpnPattern::Range(Vec::new())
            } else {
                // 1. Sort the ranges by their start value.
                intervals.sort_by_key(|r| *r.start());

                // 2. Merge them into a new vector.
                let mut merged = Vec::new();
                let mut current_interval = intervals.remove(0);

                for next_interval in intervals {
                    // Check if the next range is adjacent or overlapping.
                    // The `+ 1` handles adjacency, e.g., `1..=5` and `6..=10`.
                    if *next_interval.start() <= current_interval.end().saturating_add(1) {
                        // If so, extend the current range to encompass the next one.
                        current_interval = *current_interval.start()
                            ..=*current_interval.end().max(next_interval.end());
                    } else {
                        // Otherwise, the current range is finished. Push it to the results
                        // and start a new current range.
                        if current_interval.end() == current_interval.start() {
                            merged.push(IpnInterval::Number(*current_interval.start()));
                        } else {
                            merged.push(IpnInterval::Range(current_interval));
                        }
                        current_interval = next_interval;
                    }
                }
                // Add the last processed range.
                if current_interval.end() == current_interval.start() {
                    merged.push(IpnInterval::Number(*current_interval.start()));
                } else {
                    merged.push(IpnInterval::Range(current_interval));
                }
                IpnPattern::Range(merged)
            }
        })
        .parse_next(input)
}

// ipn-interval = ipn-decimal [ ("-" ipn-decimal) / "+" ]
fn parse_ipn_interval(input: &mut &str) -> ModalResult<RangeInclusive<u32>> {
    (
        dec_uint,
        opt(alt(("+".map(|_| u32::MAX), preceded("-", dec_uint)))),
    )
        .map(|(start, end)| {
            let end = end.unwrap_or(start);
            start.min(end)..=start.max(end)
        })
        .parse_next(input)
}
