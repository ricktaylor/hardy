use super::*;
use eid_pattern::*;
use std::{collections::HashMap, sync::Arc};

mod dtn_pattern_map;
mod ipn_pattern_map;

type Entries<I, T> = HashMap<I, T>;

#[derive(Default)]
pub struct EidPatternMap<I, T>
where
    I: Eq + std::hash::Hash + Clone,
    T: Clone,
{
    exact: HashMap<Eid, Entries<I, T>>,
    any: Entries<I, T>,
    dtn_map: dtn_pattern_map::DtnPatternMap<I, Arc<T>>,
    ipn_map: ipn_pattern_map::IpnPatternMap<I, Arc<T>>,
    numeric_schemes: HashMap<u64, Entries<I, Arc<T>>>,
    text_schemes: HashMap<String, Entries<I, Arc<T>>>,
}

impl<I, T> EidPatternMap<I, T>
where
    I: Eq + std::hash::Hash + Clone,
    T: Clone,
{
    pub fn insert(&mut self, key: &EidPattern, id: I, value: T) -> Option<T> {
        if let Some(eid) = key.is_exact() {
            self.exact.entry(eid).or_default().insert(id, value)
        } else {
            match key {
                EidPattern::Any => self.any.insert(id, value),
                EidPattern::Set(v) => {
                    let mut prev = None;
                    let value = Arc::new(value);
                    for i in v {
                        prev = self.insert_item(i, id.clone(), value.clone()).or(prev);
                    }
                    prev.map(|r| (*r).clone())
                }
            }
        }
    }

    fn insert_item(&mut self, item: &EidPatternItem, id: I, value: Arc<T>) -> Option<Arc<T>> {
        match item {
            EidPatternItem::DtnPatternItem(d) => self.dtn_map.insert(d, id, value),
            EidPatternItem::IpnPatternItem(i) => self.ipn_map.insert(i, id, value),
            EidPatternItem::AnyNumericScheme(n) => self
                .numeric_schemes
                .entry(*n)
                .or_default()
                .insert(id, value),
            EidPatternItem::AnyTextScheme(s) => self
                .text_schemes
                .entry(s.clone())
                .or_default()
                .insert(id, value),
        }
    }

    pub fn remove(&mut self, key: &EidPattern, id: &I) -> Option<T> {
        if let Some(eid) = key.is_exact() {
            self.exact.get_mut(&eid).and_then(|e| e.remove(id))
        } else {
            match key {
                EidPattern::Any => self.any.remove(id),
                EidPattern::Set(v) => {
                    let mut prev = None;
                    for i in v {
                        prev = self.remove_item(i, id).or(prev);
                    }
                    prev.map(|r| (*r).clone())
                }
            }
        }
    }

    fn remove_item(&mut self, item: &EidPatternItem, id: &I) -> Option<Arc<T>> {
        match item {
            EidPatternItem::DtnPatternItem(d) => self.dtn_map.remove(d, id),
            EidPatternItem::IpnPatternItem(i) => self.ipn_map.remove(i, id),
            EidPatternItem::AnyNumericScheme(n) => {
                if let Some(e) = self.numeric_schemes.get_mut(n) {
                    let r = e.remove(id);
                    if r.is_some() && e.is_empty() {
                        self.numeric_schemes.remove(n);
                    }
                    r
                } else {
                    None
                }
            }
            EidPatternItem::AnyTextScheme(s) => {
                if let Some(e) = self.text_schemes.get_mut(s) {
                    let r = e.remove(id);
                    if r.is_some() && e.is_empty() {
                        self.text_schemes.remove(s);
                    }
                    r
                } else {
                    None
                }
            }
        }
    }

    pub fn find(&self, eid: &Eid) -> Vec<&T> {
        // Get "anys"
        let mut results = self.any.values().collect::<Vec<&T>>();

        // Get "exacts"
        if let Some(m) = self.exact.get(eid) {
            results.extend(m.values().collect::<Vec<&T>>());
        }

        // Pattern match on EID type
        match eid {
            Eid::Null => {}
            Eid::LocalNode { service_number } => {
                results.extend(
                    self.ipn_map
                        .find(0, u32::MAX, *service_number)
                        .into_iter()
                        .map(|r| r.as_ref()),
                );
            }
            Eid::Ipn2 {
                allocator_id,
                node_number,
                service_number,
            }
            | Eid::Ipn3 {
                allocator_id,
                node_number,
                service_number,
            } => {
                results.extend(
                    self.ipn_map
                        .find(*allocator_id, *node_number, *service_number)
                        .into_iter()
                        .map(|r| r.as_ref()),
                );
            }
            Eid::Dtn { node_name, demux } => {
                results.extend(
                    self.dtn_map
                        .find(node_name, demux)
                        .into_iter()
                        .map(|r| r.as_ref()),
                );
            }
        }
        results
    }
}
