use super::*;
use dtn_pattern::*;

#[derive(Debug)]
struct Node<V: PartialEq> {
    exact: HashMap<Box<str>, Box<Node<V>>>,
    regex: HashMap<HashableRegEx, Box<Node<V>>>,
    any: Option<Box<Node<V>>>,
    all: Vec<Arc<V>>,
    values: Vec<Arc<V>>,
}

impl<V: PartialEq> Default for Node<V> {
    fn default() -> Self {
        Self {
            exact: Default::default(),
            regex: Default::default(),
            any: Default::default(),
            all: Default::default(),
            values: Default::default(),
        }
    }
}

impl<V: PartialEq> Node<V> {
    fn is_empty(&self) -> bool {
        self.all.is_empty()
            && self.values.is_empty()
            && self.any.is_none()
            && self.exact.is_empty()
            && self.regex.is_empty()
    }

    fn find_last(&self) -> impl Iterator<Item = &Arc<V>> {
        self.all.iter().chain(self.values.iter())
    }

    fn find(
        &self,
        s: &str,
    ) -> (
        impl Iterator<Item = &Box<Node<V>>>,
        impl Iterator<Item = &Arc<V>>,
    ) {
        (
            self.any
                .as_ref()
                .into_iter()
                .chain(self.exact.get(s))
                .chain({
                    self.regex
                        .iter()
                        .filter_map(|(k, v)| k.is_match(s).then_some(v))
                }),
            self.all.iter(),
        )
    }
}

#[derive(Debug)]
pub struct DtnPatternMap<V: Eq + std::hash::Hash> {
    root: Node<V>,
    none: Vec<Arc<V>>,
}

impl<V: Eq + std::hash::Hash> DtnPatternMap<V> {
    pub fn is_empty(&self) -> bool {
        self.root.is_empty() && self.none.is_empty()
    }

    pub fn insert(&mut self, pattern: DtnPatternItem, value: Arc<V>) {
        match pattern {
            DtnPatternItem::DtnNone => self.none.push(value),
            DtnPatternItem::DtnSsp(pattern) => self.insert_item(pattern, value),
        }
    }

    fn insert_item(&mut self, pattern: DtnSsp, value: Arc<V>) {
        let mut node = match pattern.node_name {
            DtnNodeNamePattern::PatternMatch(PatternMatch::Exact(s)) => {
                self.root.exact.entry(s).or_default()
            }
            DtnNodeNamePattern::PatternMatch(PatternMatch::Regex(r)) => {
                self.root.regex.entry(r).or_default()
            }
            DtnNodeNamePattern::MultiWildcard => {
                self.root.any.get_or_insert_with(Box::default).as_mut()
            }
        };

        for s in pattern.demux {
            node = match s {
                DtnSinglePattern::PatternMatch(PatternMatch::Exact(s)) => {
                    node.exact.entry(s).or_default()
                }
                DtnSinglePattern::PatternMatch(PatternMatch::Regex(r)) => {
                    node.regex.entry(r).or_default()
                }
                DtnSinglePattern::Wildcard => node.any.get_or_insert_with(Box::default),
            };
        }

        if pattern.last_wild {
            node.all.push(value);
        } else {
            node.values.push(value);
        }
    }

    pub fn remove(&mut self, pattern: &DtnPatternItem, results: &mut HashSet<Arc<V>>) {
        // Handle Null first
        let DtnPatternItem::DtnSsp(pattern) = pattern else {
            return results.extend(std::mem::take(&mut self.none));
        };

        results.extend(std::mem::take(&mut self.root.all));

        let mut node = match &pattern.node_name {
            DtnNodeNamePattern::PatternMatch(PatternMatch::Exact(s)) => self.root.exact.get_mut(s),
            DtnNodeNamePattern::PatternMatch(PatternMatch::Regex(r)) => self.root.regex.get_mut(r),
            DtnNodeNamePattern::MultiWildcard => self.root.any.as_mut(),
        };

        for s in &pattern.demux {
            let Some(n) = node else {
                break;
            };

            results.extend(std::mem::take(&mut n.all));

            node = match s {
                DtnSinglePattern::PatternMatch(PatternMatch::Exact(s)) => n.exact.get_mut(s),
                DtnSinglePattern::PatternMatch(PatternMatch::Regex(r)) => n.regex.get_mut(r),
                DtnSinglePattern::Wildcard => n.any.as_mut(),
            };
        }

        if let Some(n) = node {
            results.extend(std::mem::take(&mut n.values));
        }
    }

    pub fn remove_if<F: Fn(&V) -> bool>(
        &mut self,
        pattern: &DtnPatternItem,
        f: F,
        results: &mut HashSet<Arc<V>>,
    ) {
        // Handle Null first
        let DtnPatternItem::DtnSsp(pattern) = pattern else {
            return results.extend(self.none.extract_if(.., |v| f(v)));
        };

        results.extend(self.root.all.extract_if(.., |v| f(v)));

        let mut node = match &pattern.node_name {
            DtnNodeNamePattern::PatternMatch(PatternMatch::Exact(s)) => self.root.exact.get_mut(s),
            DtnNodeNamePattern::PatternMatch(PatternMatch::Regex(r)) => self.root.regex.get_mut(r),
            DtnNodeNamePattern::MultiWildcard => self.root.any.as_mut(),
        };

        for s in &pattern.demux {
            let Some(n) = node else {
                break;
            };

            results.extend(n.all.extract_if(.., |v| f(v)));

            node = match s {
                DtnSinglePattern::PatternMatch(PatternMatch::Exact(s)) => n.exact.get_mut(s),
                DtnSinglePattern::PatternMatch(PatternMatch::Regex(r)) => n.regex.get_mut(r),
                DtnSinglePattern::Wildcard => n.any.as_mut(),
            };
        }

        if let Some(n) = node {
            results.extend(n.values.extract_if(.., |v| f(v)));
        }
    }

    pub fn find(&self, eid: &Eid) -> impl Iterator<Item = &Arc<V>> {
        (if let Eid::Dtn { node_name, demux } = eid {
            Some({
                let (nodes, root_results) = self.root.find(node_name);
                let mut nodes = nodes.collect::<Vec<_>>();
                let mut results = Vec::new();
                for s in demux {
                    let mut subnodes = Vec::new();
                    for node in nodes {
                        let (n, v) = node.find(s);
                        subnodes.extend(n);
                        results.push(v);
                    }
                    nodes = subnodes;
                }

                root_results
                    .chain(results.into_iter().flatten())
                    .chain(nodes.into_iter().flat_map(|n| n.find_last()))
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
            root: Default::default(),
            none: Default::default(),
        }
    }
}
