use super::*;

#[derive(Clone)]
struct HashableRegEx(regex::Regex);

impl std::cmp::PartialEq for HashableRegEx {
    fn eq(&self, other: &Self) -> bool {
        self.0.as_str() == other.0.as_str()
    }
}

impl std::cmp::Eq for HashableRegEx {}

impl std::hash::Hash for HashableRegEx {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.0.as_str().hash(state);
    }
}

struct Matches<'a, I, T>
where
    I: Eq + std::hash::Hash + Clone + Default,
    T: Clone + Default,
{
    sub_nodes: Vec<&'a Node<I, T>>,
    values: Vec<&'a T>,
}

#[derive(Default, Clone)]
struct Node<I, T>
where
    I: Eq + std::hash::Hash + Clone + Default,
    T: Clone + Default,
{
    exact: HashMap<String, Box<Node<I, T>>>,
    regex: HashMap<HashableRegEx, Box<Node<I, T>>>,
    any: Option<Box<Node<I, T>>>,
    all: Entries<I, T>,
    values: Entries<I, T>,
}

impl<I, T> Node<I, T>
where
    I: Eq + std::hash::Hash + Clone + Default,
    T: Clone + Default,
{
    fn find(&self, s: &str) -> Matches<I, T> {
        let mut m = Matches {
            sub_nodes: Vec::new(),
            values: self.all.values().collect(),
        };

        if let Some(n) = self.exact.get(s) {
            m.sub_nodes.push(n);
        }

        if let Some(any) = &self.any {
            m.sub_nodes.push(any)
        }

        for (k, v) in &self.regex {
            if k.0.is_match(s) {
                m.sub_nodes.push(v)
            }
        }
        m
    }
}

#[derive(Default, Clone)]
pub struct DtnPatternMap<I, T>
where
    I: Eq + std::hash::Hash + Clone + Default,
    T: Clone + Default,
{
    auths: Node<I, T>,
}

impl<I, T> DtnPatternMap<I, T>
where
    I: Eq + std::hash::Hash + Clone + Default,
    T: Clone + Default,
{
    pub fn insert(&mut self, key: &DtnSsp, id: I, value: T) -> Option<T> {
        let mut demux = match &key.authority {
            DtnAuthPattern::PatternMatch(PatternMatch::Exact(s)) => {
                self.auths.exact.entry(s.clone()).or_default().as_mut()
            }
            DtnAuthPattern::PatternMatch(PatternMatch::Regex(r)) => self
                .auths
                .regex
                .entry(HashableRegEx(r.clone()))
                .or_default()
                .as_mut(),
            DtnAuthPattern::MultiWildcard => {
                self.auths.any.get_or_insert_with(Box::default).as_mut()
            }
        };

        for s in &key.singles {
            demux = match s {
                DtnSinglePattern::PatternMatch(PatternMatch::Exact(s)) => {
                    demux.exact.entry(s.clone()).or_default().as_mut()
                }
                DtnSinglePattern::PatternMatch(PatternMatch::Regex(r)) => demux
                    .regex
                    .entry(HashableRegEx(r.clone()))
                    .or_default()
                    .as_mut(),
                DtnSinglePattern::Wildcard => demux.any.get_or_insert_with(Box::default).as_mut(),
            };
        }

        demux = match &key.last {
            DtnLastPattern::Single(DtnSinglePattern::PatternMatch(PatternMatch::Exact(s))) => {
                demux.exact.entry(s.clone()).or_default()
            }
            DtnLastPattern::Single(DtnSinglePattern::PatternMatch(PatternMatch::Regex(r))) => {
                demux.regex.entry(HashableRegEx(r.clone())).or_default()
            }
            DtnLastPattern::Single(DtnSinglePattern::Wildcard) => {
                demux.any.get_or_insert_with(Box::default)
            }
            DtnLastPattern::MultiWildcard => return demux.all.insert(id, value),
        };

        demux.values.insert(id, value)
    }

    pub fn remove(&mut self, key: &DtnSsp, id: &I) -> Option<T> {
        let mut demux = match &key.authority {
            DtnAuthPattern::PatternMatch(PatternMatch::Exact(s)) => self.auths.exact.get_mut(s),
            DtnAuthPattern::PatternMatch(PatternMatch::Regex(r)) => {
                self.auths.regex.get_mut(&HashableRegEx(r.clone()))
            }
            DtnAuthPattern::MultiWildcard => self.auths.any.as_mut(),
        }?;

        for s in &key.singles {
            demux = match s {
                DtnSinglePattern::PatternMatch(PatternMatch::Exact(s)) => demux.exact.get_mut(s),
                DtnSinglePattern::PatternMatch(PatternMatch::Regex(r)) => {
                    demux.regex.get_mut(&HashableRegEx(r.clone()))
                }
                DtnSinglePattern::Wildcard => demux.any.as_mut(),
            }?;
        }

        match &key.last {
            DtnLastPattern::Single(DtnSinglePattern::PatternMatch(PatternMatch::Exact(s))) => {
                demux.exact.get_mut(s)
            }
            DtnLastPattern::Single(DtnSinglePattern::PatternMatch(PatternMatch::Regex(r))) => {
                demux.regex.get_mut(&HashableRegEx(r.clone()))
            }
            DtnLastPattern::Single(DtnSinglePattern::Wildcard) => demux.any.as_mut(),
            DtnLastPattern::MultiWildcard => return demux.all.remove(id),
        }?
        .values
        .remove(id)
    }

    pub fn find(&self, node_name: &str, demux: &[String]) -> Vec<&T> {
        let m = self.auths.find(node_name);
        let mut nodes = m.sub_nodes;
        let mut values = m.values;

        for s in demux {
            let mut sub_nodes = Vec::new();
            for n in &nodes {
                let m = n.find(s);
                sub_nodes.extend(m.sub_nodes);
                values.extend(m.values);
            }
            nodes = sub_nodes;
        }

        for n in &nodes {
            values.extend(n.values.values());
        }

        values
    }
}
