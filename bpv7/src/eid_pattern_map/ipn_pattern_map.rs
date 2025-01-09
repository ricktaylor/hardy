use super::*;
use std::ops::RangeInclusive;

#[derive(Clone)]
enum Interval {
    All,
    Exact(u32),
    Range(RangeInclusive<u32>),
}

fn unpack_intervals(item: &IpnPattern) -> Vec<Interval> {
    match item {
        IpnPattern::Wildcard => vec![Interval::All],
        IpnPattern::Range(v) => v
            .iter()
            .map(|i| match i {
                IpnInterval::Number(n) => Interval::Exact(*n),
                IpnInterval::Range(r) => Interval::Range(r.clone()),
            })
            .collect(),
    }
}

/*
TODO: This is an inefficient implementation
Ideally this would be rewritten as some kind of 3 dimensional kd-tree
*/
#[derive(Default, Debug, Clone)]
struct IntervalMap<T>
where
    T: Clone,
{
    any: Option<T>,
    exact: HashMap<u32, T>,
    ranges: Vec<(RangeInclusive<u32>, T)>,
}

impl<T> IntervalMap<T>
where
    T: Clone,
{
    fn insert(&mut self, i: &Interval, f: impl FnOnce() -> T) -> &mut T {
        match i {
            Interval::All => self.any.get_or_insert_with(f),
            Interval::Exact(n) => self.exact.entry(*n).or_insert_with(f),
            Interval::Range(r) => {
                let idx = if let Some(idx) = self.ranges.iter().position(|(r2, _)| r == r2) {
                    idx
                } else {
                    self.ranges.push((r.clone(), f()));
                    self.ranges.len() - 1
                };
                &mut self.ranges[idx].1
            }
        }
    }

    fn find(&self, n: u32) -> Vec<&T> {
        let mut results = Vec::new();
        if let Some(r) = &self.any {
            results.push(r);
        }
        if let Some(r) = self.exact.get(&n) {
            results.push(r);
        }
        for (r, t) in &self.ranges {
            if r.contains(&n) {
                results.push(t);
            }
        }
        results
    }

    fn lookup(&mut self, i: &Interval) -> Option<&mut T> {
        match i {
            Interval::All => self.any.as_mut(),
            Interval::Exact(n) => self.exact.get_mut(n),
            Interval::Range(r) => self
                .ranges
                .iter_mut()
                .find(|(r2, _)| r == r2)
                .map(|(_, t)| t),
        }
    }

    fn remove(&mut self, i: &Interval) {
        match i {
            Interval::All => self.any = None,
            Interval::Exact(n) => {
                self.exact.remove(n);
            }
            Interval::Range(r) => {
                self.ranges = std::mem::take(&mut self.ranges)
                    .into_iter()
                    .filter(|(r2, _)| r2 == r)
                    .collect()
            }
        }
    }

    fn is_empty(&self) -> bool {
        self.any.is_none() && self.exact.is_empty() && self.ranges.is_empty()
    }
}

#[derive(Default, Debug, Clone)]
pub struct IpnPatternMap<I, T>
where
    I: Eq + std::hash::Hash + Clone,
    T: Clone,
{
    intervals: IntervalMap<IntervalMap<IntervalMap<Entries<I, T>>>>,
}

impl<I, T> IpnPatternMap<I, T>
where
    I: Eq + std::hash::Hash + Clone,
    T: Clone,
{
    pub fn insert(&mut self, key: &IpnPatternItem, id: I, value: T) -> Option<T> {
        let allocators = unpack_intervals(&key.allocator_id);
        let node_numbers = unpack_intervals(&key.node_number);
        let service_numbers = unpack_intervals(&key.service_number);

        let mut prev = None;
        for i1 in allocators {
            for i2 in node_numbers.iter() {
                for i3 in service_numbers.iter() {
                    prev = self
                        .intervals
                        .insert(&i1, IntervalMap::default)
                        .insert(i2, IntervalMap::default)
                        .insert(i3, HashMap::default)
                        .insert(id.clone(), value.clone())
                        .or(prev);
                }
            }
        }
        prev
    }

    pub fn remove<J>(&mut self, key: &IpnPatternItem, id: &J) -> Option<T>
    where
        I: std::borrow::Borrow<J>,
        J: std::hash::Hash + Eq + ?Sized,
    {
        let allocators = unpack_intervals(&key.allocator_id);
        let node_numbers = unpack_intervals(&key.node_number);
        let service_numbers = unpack_intervals(&key.service_number);

        let mut prev = None;
        let mut r1 = false;
        for i1 in allocators {
            if let Some(m1) = self.intervals.lookup(&i1) {
                for i2 in node_numbers.iter() {
                    if let Some(m2) = m1.lookup(i2) {
                        let mut r2 = false;
                        for i3 in service_numbers.iter() {
                            if let Some(h) = m2.lookup(i3) {
                                if let Some(p) = h.remove(id) {
                                    if h.is_empty() {
                                        m2.remove(i3);
                                        r2 = true;
                                    }
                                    prev = Some(p);
                                }
                            }
                        }
                        if r2 && m2.is_empty() {
                            m1.remove(i2);
                            r1 = true;
                        }
                    }
                }
                if r1 && m1.is_empty() {
                    self.intervals.remove(&i1);
                }
            }
        }
        prev
    }

    pub fn find(&self, allocator_id: u32, node_number: u32, service_number: u32) -> Vec<&T> {
        let mut results = Vec::new();
        for i1 in self.intervals.find(allocator_id) {
            for i2 in i1.find(node_number) {
                for i3 in i2.find(service_number) {
                    results.extend(i3.values())
                }
            }
        }
        results
    }
}
