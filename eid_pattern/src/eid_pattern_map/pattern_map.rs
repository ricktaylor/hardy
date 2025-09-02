use super::*;

#[derive(Debug)]
pub struct PatternMap<V: Eq + std::hash::Hash> {
    exact: HashMap<Eid, Vec<Arc<V>>>,
    any: Vec<Arc<V>>,
    ipn_map: ipn_pattern_map::IpnPatternMap<V>,
    numeric_schemes: HashMap<u64, Vec<Arc<V>>>,
    text_schemes: HashMap<String, Vec<Arc<V>>>,

    #[cfg(feature = "dtn-pat-item")]
    dtn_map: dtn_pattern_map::DtnPatternMap<V>,
}

impl<V: Eq + std::hash::Hash> PatternMap<V> {
    pub fn is_empty(&self) -> bool {
        if !self.exact.is_empty()
            || !self.any.is_empty()
            || !self.ipn_map.is_empty()
            || !self.numeric_schemes.is_empty()
            || !self.text_schemes.is_empty()
        {
            return false;
        }

        #[cfg(feature = "dtn-pat-item")]
        if !self.dtn_map.is_empty() {
            return false;
        }

        true
    }

    pub fn insert(&mut self, pattern: &EidPattern, value: V) {
        let value = Arc::new(value);
        match pattern {
            EidPattern::Any => {
                self.any.push(value);
            }
            EidPattern::Set(patterns) => {
                for p in patterns {
                    self.insert_item(p, value.clone());
                }
            }
        }
    }

    fn insert_item(&mut self, pattern: &EidPatternItem, value: Arc<V>) {
        if let Some(eid) = pattern.try_to_eid() {
            self.exact.entry(eid).or_default().push(value);
        } else {
            match pattern {
                EidPatternItem::IpnPatternItem(pattern) => self.ipn_map.insert(pattern, value),
                EidPatternItem::AnyNumericScheme(n) => {
                    self.numeric_schemes.entry(*n).or_default().push(value);
                }
                EidPatternItem::AnyTextScheme(s) => {
                    let h = self.text_schemes.get_mut(s);
                    if let Some(h) = h {
                        h.push(value);
                    } else {
                        self.text_schemes.insert(s.clone(), vec![value]);
                    }
                }

                #[cfg(feature = "dtn-pat-item")]
                EidPatternItem::DtnPatternItem(pattern) => self.dtn_map.insert(pattern, value),
            }
        }
    }

    pub fn remove(&mut self, pattern: &EidPattern) -> impl Iterator<Item = V> {
        let mut results = HashSet::new();
        match pattern {
            EidPattern::Any => results.extend(std::mem::take(&mut self.any)),
            EidPattern::Set(patterns) => {
                for p in patterns {
                    self.remove_item(p, &mut results);
                }
            }
        }
        results
            .into_iter()
            .filter_map(|v| Arc::<V>::try_unwrap(v).ok())
    }

    fn remove_item(&mut self, pattern: &EidPatternItem, results: &mut HashSet<Arc<V>>) {
        if let Some(eid) = pattern.try_to_eid() {
            results.extend(self.exact.remove(&eid).into_iter().flatten());
        } else {
            match pattern {
                EidPatternItem::IpnPatternItem(pattern) => self.ipn_map.remove(pattern, results),
                EidPatternItem::AnyNumericScheme(n) => {
                    results.extend(self.numeric_schemes.remove(n).into_iter().flatten());
                }
                EidPatternItem::AnyTextScheme(s) => {
                    results.extend(self.text_schemes.remove(s).into_iter().flatten());
                }

                #[cfg(feature = "dtn-pat-item")]
                EidPatternItem::DtnPatternItem(pattern) => self.dtn_map.remove(pattern, results),
            }
        }
    }

    pub fn remove_if<F: Fn(&V) -> bool>(
        &mut self,
        pattern: &EidPattern,
        f: F,
    ) -> impl Iterator<Item = V> {
        let mut results = HashSet::new();
        match pattern {
            EidPattern::Any => results.extend(self.any.extract_if(.., |v| f(v))),
            EidPattern::Set(patterns) => {
                for p in patterns {
                    self.remove_item_if(p, |v| f(v), &mut results);
                }
            }
        }
        results
            .into_iter()
            .filter_map(|v| Arc::<V>::try_unwrap(v).ok())
    }

    fn remove_item_if<F: Fn(&V) -> bool>(
        &mut self,
        pattern: &EidPatternItem,
        f: F,
        results: &mut HashSet<Arc<V>>,
    ) {
        if let Some(eid) = pattern.try_to_eid() {
            if let hash_map::Entry::Occupied(mut o) = self.exact.entry(eid) {
                results.extend(o.get_mut().extract_if(.., |v| f(v)));
                if o.get().is_empty() {
                    o.remove();
                }
            }
        } else {
            match pattern {
                EidPatternItem::IpnPatternItem(pattern) => {
                    self.ipn_map.remove_if(pattern, f, results)
                }
                EidPatternItem::AnyNumericScheme(n) => {
                    if let hash_map::Entry::Occupied(mut o) = self.numeric_schemes.entry(*n) {
                        results.extend(o.get_mut().extract_if(.., |v| f(v)));
                        if o.get().is_empty() {
                            o.remove();
                        }
                    }
                }
                EidPatternItem::AnyTextScheme(s) => {
                    if let hash_map::Entry::Occupied(mut o) = self.text_schemes.entry(s.into()) {
                        results.extend(o.get_mut().extract_if(.., |v| f(v)));
                        if o.get().is_empty() {
                            o.remove();
                        }
                    }
                }

                #[cfg(feature = "dtn-pat-item")]
                EidPatternItem::DtnPatternItem(pattern) => {
                    self.dtn_map.remove_if(pattern, f, results)
                }
            }
        }
    }

    pub fn find(&self, eid: &Eid) -> impl Iterator<Item = &V> {
        self.any
            .iter()
            .chain(self.exact.get(eid).map(|v| v.iter()).into_iter().flatten())
            .chain(
                match eid {
                    #[cfg(not(feature = "dtn-pat-item"))]
                    Eid::Dtn { .. } => Some(&1),
                    Eid::Unknown { scheme, .. } => Some(scheme),
                    _ => None,
                }
                .and_then(|scheme| self.numeric_schemes.get(scheme).map(|v| v.iter()))
                .into_iter()
                .flatten(),
            )
            .chain({
                let i = self.ipn_map.find(eid);

                #[cfg(feature = "dtn-pat-item")]
                let i = i.chain(self.dtn_map.find(eid));

                i
            })
            .map(AsRef::as_ref)
    }
}

impl<V: Eq + std::hash::Hash> Default for PatternMap<V> {
    fn default() -> Self {
        Self {
            exact: Default::default(),
            any: Default::default(),
            ipn_map: Default::default(),
            numeric_schemes: Default::default(),
            text_schemes: Default::default(),

            #[cfg(feature = "dtn-pat-item")]
            dtn_map: Default::default(),
        }
    }
}
