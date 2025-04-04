use super::*;
use std::collections::HashMap;

mod dtn_pattern_map;
mod ipn_pattern_map;

type Entries<I, T> = HashMap<I, T>;

#[derive(Default, Debug, Clone)]
pub struct EidPatternMap<I, T>
where
    I: Eq + std::hash::Hash + Clone + Default,
    T: Clone + Default,
{
    exact: HashMap<Eid, Entries<I, T>>,
    any: Entries<I, T>,
    none: Entries<I, T>,
    dtn_map: dtn_pattern_map::DtnPatternMap<I, T>,
    ipn_map: ipn_pattern_map::IpnPatternMap<I, T>,
    numeric_schemes: HashMap<u64, Entries<I, T>>,
    //text_schemes: HashMap<String, Entries<I, T>>,
}

impl<I, T> EidPatternMap<I, T>
where
    I: Eq + std::hash::Hash + Clone + Default,
    T: Clone + Default,
{
    pub fn new() -> Self {
        Default::default()
    }

    pub fn insert(&mut self, key: &EidPattern, id: I, value: T) -> Option<T> {
        if let Some(eid) = key.is_exact() {
            self.exact.entry(eid).or_default().insert(id, value)
        } else {
            match key {
                EidPattern::Any => self.any.insert(id, value),
                EidPattern::Set(v) => {
                    let mut prev = None;
                    for i in v {
                        prev = self.insert_item(i, id.clone(), value.clone()).or(prev);
                    }
                    prev
                }
            }
        }
    }

    fn insert_item(&mut self, item: &EidPatternItem, id: I, value: T) -> Option<T> {
        let mut prev = None;
        if let Some(eid) = item.is_exact() {
            prev = self
                .exact
                .entry(eid)
                .or_default()
                .insert(id.clone(), value.clone());
        }

        match item {
            EidPatternItem::DtnPatternItem(DtnPatternItem::None) => {
                prev = self.none.insert(id, value).or(prev);
            }
            EidPatternItem::DtnPatternItem(DtnPatternItem::DtnSsp(ssp)) => {
                prev = self.dtn_map.insert(ssp, id, value).or(prev);
            }
            EidPatternItem::IpnPatternItem(i) => {
                if i.is_match(&Eid::Null) {
                    self.none.insert(id.clone(), value.clone());
                }
                prev = self.ipn_map.insert(i, id, value).or(prev);
            }
            EidPatternItem::AnyNumericScheme(n) => {
                prev = self
                    .numeric_schemes
                    .entry(*n)
                    .or_default()
                    .insert(id, value)
                    .or(prev);
            }
            EidPatternItem::AnyTextScheme(_s) => {
                /*prev = self
                .text_schemes
                .entry(s.clone())
                .or_default()
                .insert(id, value)
                .or(prev);*/
            }
        }
        prev
    }

    pub fn remove<J>(&mut self, key: &EidPattern, id: &J) -> Option<T>
    where
        I: std::borrow::Borrow<J>,
        J: std::hash::Hash + Eq + ?Sized,
    {
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
                    prev
                }
            }
        }
    }

    fn remove_item<J>(&mut self, item: &EidPatternItem, id: &J) -> Option<T>
    where
        I: std::borrow::Borrow<J>,
        J: std::hash::Hash + Eq + ?Sized,
    {
        if let Some(eid) = item.is_exact() {
            self.exact.get_mut(&eid).and_then(|e| e.remove(id));
        }

        match item {
            EidPatternItem::DtnPatternItem(DtnPatternItem::None) => self.none.remove(id),
            EidPatternItem::DtnPatternItem(DtnPatternItem::DtnSsp(ssp)) => {
                self.dtn_map.remove(ssp, id)
            }
            EidPatternItem::IpnPatternItem(i) => {
                if i.is_match(&Eid::Null) {
                    self.none.remove(id);
                }
                self.ipn_map.remove(i, id)
            }
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
            EidPatternItem::AnyTextScheme(_s) => {
                /*if let Some(e) = self.text_schemes.get_mut(s) {
                    let r = e.remove(id);
                    if r.is_some() && e.is_empty() {
                        self.text_schemes.remove(s);
                    }
                    r
                } else*/
                None
            }
        }
    }

    pub fn find(&self, eid: &Eid) -> Vec<&T> {
        // Get "anys"
        let mut results = self.any.values().collect::<Vec<&T>>();

        // Get "exacts"
        if let Some(m) = self.exact.get(eid) {
            results.extend(m.values());
        }

        // Pattern match on EID type
        match eid {
            Eid::Null => {
                results.extend(self.none.values());
            }
            Eid::LocalNode { service_number } => {
                results.extend(self.ipn_map.find(0, u32::MAX, *service_number));
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
                results.extend(
                    self.ipn_map
                        .find(*allocator_id, *node_number, *service_number),
                );
            }
            Eid::Dtn { node_name, demux } => {
                results.extend(self.dtn_map.find(node_name, demux));
            }
            Eid::Unknown { scheme, .. } => {
                if let Some(v) = self.numeric_schemes.get(scheme) {
                    results.extend(v.values())
                }
            }
        }
        results
    }
}
