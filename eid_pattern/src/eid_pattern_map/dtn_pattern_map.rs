use super::*;
use dtn_pattern::*;

#[allow(clippy::type_complexity)]
#[derive(Debug)]
pub struct DtnPatternMap<V: Eq + std::hash::Hash> {
    none: Vec<Arc<V>>,
    all: Vec<Arc<V>>,
    exact: HashMap<Box<str>, HashMap<Box<str>, Vec<Arc<V>>>>,
    glob: HashMap<glob::Pattern, Vec<Arc<V>>>,
}

impl<V: Eq + std::hash::Hash> DtnPatternMap<V> {
    pub fn is_empty(&self) -> bool {
        self.none.is_empty() && self.all.is_empty() && self.exact.is_empty() && self.glob.is_empty()
    }

    pub fn insert(&mut self, pattern: DtnPatternItem, value: Arc<V>) {
        match pattern {
            DtnPatternItem::None => self.none.push(value),
            DtnPatternItem::All => self.all.push(value),
            DtnPatternItem::Glob(pattern) => match self.glob.entry(pattern) {
                hash_map::Entry::Occupied(mut o) => o.get_mut().push(value),
                hash_map::Entry::Vacant(v) => {
                    v.insert(vec![value]);
                }
            },
            DtnPatternItem::Exact(node_name, demux) => match self.exact.entry(node_name) {
                hash_map::Entry::Vacant(v) => {
                    v.insert([(demux, vec![value])].into());
                }
                hash_map::Entry::Occupied(mut o) => match o.get_mut().entry(demux) {
                    hash_map::Entry::Vacant(v) => {
                        v.insert(vec![value]);
                    }
                    hash_map::Entry::Occupied(mut o) => {
                        o.get_mut().push(value);
                    }
                },
            },
        }
    }

    pub fn remove(&mut self, pattern: &DtnPatternItem, results: &mut HashSet<Arc<V>>) {
        match pattern {
            DtnPatternItem::None => results.extend(core::mem::take(&mut self.none)),
            DtnPatternItem::All => results.extend(core::mem::take(&mut self.all)),
            DtnPatternItem::Exact(node_name, demux) => {
                if self
                    .exact
                    .get_mut(node_name)
                    .map(|e| {
                        if let Some(v) = e.remove(demux) {
                            results.extend(v);
                            e.is_empty()
                        } else {
                            false
                        }
                    })
                    .unwrap_or(false)
                {
                    self.exact.remove(node_name);
                }
            }
            DtnPatternItem::Glob(pattern) => {
                if let Some(v) = self.glob.remove(pattern) {
                    results.extend(v);
                }
            }
        }
    }

    pub fn remove_if<F: Fn(&V) -> bool>(
        &mut self,
        pattern: &DtnPatternItem,
        f: F,
        results: &mut HashSet<Arc<V>>,
    ) {
        match pattern {
            DtnPatternItem::None => results.extend(self.none.extract_if(.., |v| f(v))),
            DtnPatternItem::All => results.extend(self.all.extract_if(.., |v| f(v))),
            DtnPatternItem::Exact(node_name, demux) => {
                if self
                    .exact
                    .get_mut(node_name)
                    .map(|e| {
                        if e.get_mut(demux)
                            .map(|e2| {
                                results.extend(e2.extract_if(.., |v| f(v)));
                                e2.is_empty()
                            })
                            .unwrap_or(false)
                        {
                            e.remove(demux);
                            e.is_empty()
                        } else {
                            false
                        }
                    })
                    .unwrap_or(false)
                {
                    self.exact.remove(node_name);
                }
            }
            DtnPatternItem::Glob(pattern) => {
                if self
                    .glob
                    .get_mut(pattern)
                    .map(|e| {
                        results.extend(e.extract_if(.., |v| f(v)));
                        e.is_empty()
                    })
                    .unwrap_or(false)
                {
                    self.glob.remove(pattern);
                }
            }
        }
    }

    pub fn find(&self, eid: &Eid) -> impl Iterator<Item = &Arc<V>> {
        (if let Eid::Dtn { node_name, demux } = eid {
            Some({
                let full = format!("{node_name}/{demux}");
                self.all
                    .iter()
                    .chain(
                        self.exact
                            .get(node_name)
                            .and_then(|e| e.get(demux).map(|v| v.iter()))
                            .into_iter()
                            .flatten(),
                    )
                    .chain(
                        self.glob
                            .iter()
                            .filter_map(move |(k, v)| {
                                k.matches_with(
                                    &full,
                                    glob::MatchOptions {
                                        case_sensitive: false,
                                        require_literal_separator: true,
                                        require_literal_leading_dot: false,
                                    },
                                )
                                .then_some(v.iter())
                            })
                            .flatten(),
                    )
            })
        } else {
            None
        })
        .into_iter()
        .flatten()
        .chain(
            if let Eid::Null = eid {
                Some(self.none.iter())
            } else {
                None
            }
            .into_iter()
            .flatten(),
        )
    }
}

impl<V: Eq + std::hash::Hash> Default for DtnPatternMap<V> {
    fn default() -> Self {
        Self {
            none: Default::default(),
            all: Default::default(),
            exact: Default::default(),
            glob: Default::default(),
        }
    }
}
