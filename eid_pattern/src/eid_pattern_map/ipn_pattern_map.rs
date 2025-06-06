use super::*;
use ipn_pattern::*;
use std::{borrow::Cow, ops::RangeInclusive};

#[derive(Debug)]
enum Interval<'a> {
    All,
    Exact(u32),
    Range(Cow<'a, RangeInclusive<u32>>),
}

fn unpack_intervals(pattern: &IpnPattern) -> Vec<Interval<'_>> {
    match pattern {
        IpnPattern::Wildcard => vec![Interval::All],
        IpnPattern::Range(v) => v
            .iter()
            .map(|i| match i {
                IpnInterval::Number(n) => Interval::Exact(*n),
                IpnInterval::Range(r) => Interval::Range(Cow::Borrowed(r)),
            })
            .collect(),
    }
}

trait IsEmpty {
    fn is_empty(&self) -> bool;
}

impl<V> IsEmpty for Vec<Arc<V>> {
    fn is_empty(&self) -> bool {
        Vec::is_empty(self)
    }
}

#[derive(Debug, Default)]
struct IntervalMap<V> {
    any: Option<V>,
    exact: HashMap<u32, V>,

    /*
    TODO: This is an inefficient implementation
    Ideally this would be rewritten as some space division tree
    */
    ranges: Vec<(RangeInclusive<u32>, V)>,
}

impl<V: IsEmpty> IntervalMap<V> {
    fn insert(&mut self, i: &Interval, f: impl FnOnce() -> V) -> &mut V {
        match i {
            Interval::All => self.any.get_or_insert_with(f),
            Interval::Exact(n) => self.exact.entry(*n).or_insert_with(f),
            Interval::Range(r) => {
                let r = r.as_ref();
                let idx = self
                    .ranges
                    .iter()
                    .position(|(r2, _)| r == r2)
                    .unwrap_or_else(|| {
                        self.ranges.push((r.clone(), f()));
                        self.ranges.len() - 1
                    });
                &mut self.ranges[idx].1
            }
        }
    }

    fn find_mut(&mut self, i: &Interval) -> impl Iterator<Item = &mut V> {
        if let Interval::All = i {
            self.any.as_mut()
        } else {
            None
        }
        .into_iter()
        .chain({
            if let Interval::Exact(n) = i {
                self.exact.get_mut(n)
            } else {
                None
            }
        })
        .chain({
            if let Interval::Range(r) = i {
                Some(
                    self.ranges
                        .iter_mut()
                        .filter_map(move |(r2, v)| (r.as_ref() == r2).then_some(v)),
                )
            } else {
                None
            }
            .into_iter()
            .flatten()
        })
    }

    fn purge(&mut self) {
        self.any.take_if(|v| v.is_empty());
        self.exact.retain(|_, v| !v.is_empty());
        self.ranges.retain(|(_, v)| !v.is_empty());
    }

    fn find(&self, n: &u32) -> impl Iterator<Item = &V> {
        self.any.iter().chain(self.exact.get(n)).chain({
            self.ranges
                .iter()
                .filter_map(|(r, v)| r.contains(n).then_some(v))
        })
    }
}

impl<V: Eq + std::hash::Hash> IntervalMap<Vec<Arc<V>>> {
    fn remove(&mut self, i: &Interval, results: &mut HashSet<Arc<V>>) {
        match i {
            Interval::All => {
                if let Some(v) = self.any.take() {
                    results.extend(v);
                }
            }
            Interval::Exact(n) => {
                if let Some(v) = self.exact.remove(n) {
                    results.extend(v);
                }
            }
            Interval::Range(r) => self.remove_range(r, results),
        }
    }

    fn remove_if<F: Fn(&V) -> bool>(&mut self, i: &Interval, f: F, results: &mut HashSet<Arc<V>>) {
        match i {
            Interval::All => {
                if let Some(v) = &mut self.any {
                    results.extend(v.extract_if(.., |v| f(v)));
                }
            }
            Interval::Exact(n) => {
                if let hash_map::Entry::Occupied(mut o) = self.exact.entry(*n) {
                    results.extend(o.get_mut().extract_if(.., |v| f(v)));
                    if o.get().is_empty() {
                        o.remove();
                    }
                }
            }
            Interval::Range(r) => self.remove_range_if(r, f, results),
        }
    }

    fn remove_range(&mut self, source_range: &RangeInclusive<u32>, results: &mut HashSet<Arc<V>>) {
        for (_, v) in self.remove_overlaps(source_range) {
            // This range is completely within source_range
            results.extend(v);
        }
    }

    fn remove_range_if<F: Fn(&V) -> bool>(
        &mut self,
        source_range: &RangeInclusive<u32>,
        f: F,
        results: &mut HashSet<Arc<V>>,
    ) {
        for (r, mut v) in self.remove_overlaps(source_range) {
            // This range is completely within source_range
            results.extend(v.extract_if(.., |v| f(v)));
            if !v.is_empty() {
                // Put the range back
                self.ranges.push((r, v));
            }
        }
    }

    fn remove_overlaps(
        &mut self,
        source_range: &RangeInclusive<u32>,
    ) -> Vec<(RangeInclusive<u32>, Vec<Arc<V>>)> {
        // Remove all overlapping matches
        let overlaps = self
            .ranges
            .extract_if(.., |(r, _)| {
                (source_range.start() <= r.start() && source_range.end() >= r.end())
                    || (r.start() <= source_range.start() && r.end() >= source_range.end())
            })
            .collect::<Vec<_>>();

        // We may need to sub-divide and put back the overlaps
        let mut results = Vec::new();
        for (r, v) in overlaps {
            match (
                r.start().cmp(source_range.start()),
                r.end().cmp(source_range.end()),
            ) {
                (std::cmp::Ordering::Less, std::cmp::Ordering::Equal) => {
                    if *r.start() == source_range.start() - 1 {
                        self.exact.insert(*r.start(), v);
                    } else {
                        self.ranges.push((
                            RangeInclusive::new(*r.start(), *source_range.start() - 1),
                            v,
                        ));
                    }
                }
                (std::cmp::Ordering::Less, std::cmp::Ordering::Greater) => {
                    if *r.start() == source_range.start() - 1 {
                        self.exact.insert(*r.start(), v.clone());
                    } else {
                        self.ranges.push((
                            RangeInclusive::new(*r.start(), *source_range.start() - 1),
                            v.clone(),
                        ));
                    }
                    if *r.end() == source_range.end() + 1 {
                        self.exact.insert(*r.end(), v);
                    } else {
                        self.ranges
                            .push((RangeInclusive::new(*source_range.end() + 1, *r.end()), v));
                    }
                }
                (std::cmp::Ordering::Equal, std::cmp::Ordering::Greater) => {
                    if *r.end() == source_range.end() + 1 {
                        self.exact.insert(*r.end(), v);
                    } else {
                        self.ranges
                            .push((RangeInclusive::new(*source_range.end() + 1, *r.end()), v));
                    }
                }
                (std::cmp::Ordering::Equal, std::cmp::Ordering::Less)
                | (std::cmp::Ordering::Equal, std::cmp::Ordering::Equal)
                | (std::cmp::Ordering::Greater, std::cmp::Ordering::Less)
                | (std::cmp::Ordering::Greater, std::cmp::Ordering::Equal) => {
                    // This range is completely within source_range
                    results.push((r, v));
                }
                (std::cmp::Ordering::Less, std::cmp::Ordering::Less)
                | (std::cmp::Ordering::Greater, std::cmp::Ordering::Greater) => {
                    unreachable!()
                }
            }
        }
        results
    }
}

impl<V: IsEmpty> IsEmpty for IntervalMap<V> {
    fn is_empty(&self) -> bool {
        self.any.is_none() && self.exact.is_empty() && self.ranges.is_empty()
    }
}

#[derive(Debug)]
pub struct IpnPatternMap<V: Eq + std::hash::Hash> {
    intervals: IntervalMap<IntervalMap<IntervalMap<Vec<Arc<V>>>>>,
}

impl<V: Eq + std::hash::Hash> IpnPatternMap<V> {
    pub fn is_empty(&self) -> bool {
        self.intervals.is_empty()
    }

    pub fn insert(&mut self, pattern: IpnPatternItem, value: Arc<V>) {
        let allocators = unpack_intervals(&pattern.allocator_id);
        let node_numbers = unpack_intervals(&pattern.node_number);
        let service_numbers = unpack_intervals(&pattern.service_number);

        for i1 in allocators {
            for i2 in node_numbers.iter() {
                for i3 in service_numbers.iter() {
                    self.intervals
                        .insert(&i1, IntervalMap::default)
                        .insert(i2, IntervalMap::default)
                        .insert(i3, Vec::default)
                        .push(value.clone());
                }
            }
        }
    }

    pub fn remove(&mut self, pattern: &IpnPatternItem, results: &mut HashSet<Arc<V>>) {
        let allocators = unpack_intervals(&pattern.allocator_id);
        let node_numbers = unpack_intervals(&pattern.node_number);
        let service_numbers = unpack_intervals(&pattern.service_number);

        for i in allocators {
            for v in self.intervals.find_mut(&i) {
                for i in &node_numbers {
                    for v in v.find_mut(i) {
                        for i in &service_numbers {
                            v.remove(i, results);
                        }
                        v.purge();
                    }
                }
                v.purge();
            }
        }
        self.intervals.purge();
    }

    pub fn remove_if<F: Fn(&V) -> bool>(
        &mut self,
        pattern: &IpnPatternItem,
        f: F,
        results: &mut HashSet<Arc<V>>,
    ) {
        let allocators = unpack_intervals(&pattern.allocator_id);
        let node_numbers = unpack_intervals(&pattern.node_number);
        let service_numbers = unpack_intervals(&pattern.service_number);

        for i in allocators {
            for v in self.intervals.find_mut(&i) {
                for i in &node_numbers {
                    for v in v.find_mut(i) {
                        for i in &service_numbers {
                            v.remove_if(i, |v| f(v), results);
                        }
                        v.purge();
                    }
                }
                v.purge();
            }
        }
        self.intervals.purge();
    }

    pub fn find(&self, eid: &Eid) -> impl Iterator<Item = &Arc<V>> {
        match eid {
            Eid::Null => Some(self.find_inner(&0, &0, &0)),
            Eid::LocalNode { service_number } => {
                Some(self.find_inner(&0, &u32::MAX, service_number))
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
            } => Some(self.find_inner(allocator_id, node_number, service_number)),
            _ => None,
        }
        .into_iter()
        .flatten()
    }

    fn find_inner(
        &self,
        allocator_id: &u32,
        node_number: &u32,
        service_number: &u32,
    ) -> impl Iterator<Item = &Arc<V>> {
        self.intervals.find(allocator_id).flat_map(|v| {
            v.find(node_number)
                .flat_map(|v| v.find(service_number).flat_map(|h| h.iter()))
        })
    }
}

impl<V: Eq + std::hash::Hash> Default for IpnPatternMap<V> {
    fn default() -> Self {
        Self {
            intervals: Default::default(),
        }
    }
}
