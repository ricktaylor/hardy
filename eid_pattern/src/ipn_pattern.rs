use super::*;
use std::ops::RangeInclusive;
use winnow::{
    ModalResult, Parser,
    ascii::dec_uint,
    combinator::{alt, delimited, opt, preceded, separated},
};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct IpnPatternItem {
    pub(crate) allocator_id: IpnPattern,
    pub(crate) node_number: IpnPattern,
    pub(crate) service_number: IpnPattern,
}

impl IpnPatternItem {
    pub(crate) fn new_any() -> Self {
        Self {
            allocator_id: IpnPattern::Wildcard,
            node_number: IpnPattern::Wildcard,
            service_number: IpnPattern::Wildcard,
        }
    }

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
        write!(
            f,
            "{}.{}.{}",
            self.allocator_id, self.node_number, self.service_number
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum IpnPattern {
    Range(Vec<IpnInterval>),
    Wildcard,
}

impl IpnPattern {
    fn try_to_eid(&self) -> Option<u32> {
        match self {
            IpnPattern::Range(r) => {
                if r.len() != 1 {
                    None
                } else {
                    r[0].try_to_u32()
                }
            }
            IpnPattern::Wildcard => None,
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
            IpnInterval::Range(r) => write!(f, "{}-{}", r.start(), r.end()),
        }
    }
}

impl std::cmp::PartialOrd for IpnInterval {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl std::cmp::Ord for IpnInterval {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        match (self, other) {
            (IpnInterval::Number(n1), IpnInterval::Number(n2)) => n1.cmp(n2),
            (IpnInterval::Number(n), IpnInterval::Range(r)) => {
                n.cmp(r.start()).then((r.end() - r.start()).cmp(&1))
            }
            (IpnInterval::Range(r), IpnInterval::Number(n)) => {
                r.start().cmp(n).then(1.cmp(&(r.end() - r.start())))
            }
            (IpnInterval::Range(r1), IpnInterval::Range(r2)) => r1
                .start()
                .cmp(r2.start())
                .then((r1.end() - r1.start()).cmp(&(r2.end() - r2.start()))),
        }
    }
}

impl IpnInterval {
    fn try_to_u32(&self) -> Option<u32> {
        match self {
            IpnInterval::Number(n) => Some(*n),
            IpnInterval::Range(_) => None,
        }
    }
}

// ipn-pat-item = "ipn:" (ipn-ssp3 / ipn-ssp2)
// ipn-ssp3 = ipn-part-pat nbr-delim ipn-part-pat nbr-delim ipn-part-pat
// ipn-ssp2 = ipn-part-pat nbr-delim ipn-part-pat
pub(crate) fn parse_ipn_pat_item(input: &mut &[u8]) -> ModalResult<EidPatternItem> {
    preceded(
        "ipn:",
        alt((
            "**".map(|_| IpnPatternItem::new_any()),
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
fn parse_ipn_part_pat(input: &mut &[u8]) -> ModalResult<IpnPattern> {
    alt((
        "*".map(|_| IpnPattern::Wildcard),
        dec_uint.map(|v| IpnPattern::Range(vec![IpnInterval::Number(v)])),
        parse_ipn_range,
    ))
    .parse_next(input)
}

// ipn-range = "[" ipn-interval *( "," ipn-interval ) "]"
fn parse_ipn_range(input: &mut &[u8]) -> ModalResult<IpnPattern> {
    delimited("[", separated(1.., parse_ipn_interval, ","), "]")
        .map(|mut intervals: Vec<IpnInterval>| {
            // Sort
            intervals.sort();

            // Dedup
            intervals.dedup();

            // Merge intervals
            let mut i = intervals.into_iter();
            let mut intervals = Vec::new();
            let mut curr = i.next().unwrap();
            for next in i {
                match (&curr, &next) {
                    (IpnInterval::Number(n1), IpnInterval::Number(n2)) if *n2 == n1 + 1 => {
                        curr = IpnInterval::Range(*n1..=*n2);
                    }
                    (IpnInterval::Number(n), IpnInterval::Range(r)) if n == r.start() => {
                        curr = next;
                    }
                    (IpnInterval::Number(n), IpnInterval::Range(r)) if n + 1 == *r.start() => {
                        curr = IpnInterval::Range(*n..=*r.end());
                    }
                    (IpnInterval::Range(r), IpnInterval::Number(n)) if r.contains(n) => {}
                    (IpnInterval::Range(r), IpnInterval::Number(n)) if r.end() + 1 == *n => {
                        curr = IpnInterval::Range(*r.start()..=*n);
                    }
                    (IpnInterval::Range(r1), IpnInterval::Range(r2))
                        if *r2.start() <= r1.end() + 1 =>
                    {
                        curr = IpnInterval::Range(*r1.start()..=*r2.end());
                    }
                    _ => {
                        intervals.push(curr);
                        curr = next;
                    }
                }
            }
            intervals.push(curr);
            IpnPattern::Range(intervals)
        })
        .parse_next(input)
}

// ipn-interval = ipn-decimal [ "-" (ipn-decimal / "max") ]
fn parse_ipn_interval(input: &mut &[u8]) -> ModalResult<IpnInterval> {
    (
        dec_uint,
        opt(preceded("-", alt((dec_uint, "max".map(|_| u32::MAX))))),
    )
        .map(|(start, end)| {
            end.map_or_else(
                || IpnInterval::Number(start),
                |end| IpnInterval::Range(start..=end),
            )
        })
        .parse_next(input)
}
