use super::*;

#[derive(Default)]
pub struct DtnPatternMap<I, T>
where
    I: Eq + std::hash::Hash + Clone,
    T: Clone,
{
    _dummy: HashMap<I, T>,
}

impl<I, T> DtnPatternMap<I, T>
where
    I: Eq + std::hash::Hash + Clone,
    T: Clone,
{
    pub fn insert(&mut self, key: &DtnPatternItem, id: I, value: T) -> Option<T> {
        todo!()
    }

    pub fn remove(&mut self, key: &DtnPatternItem, id: &I) -> Option<T> {
        todo!()
    }

    pub fn find(&self, node_name: &str, demux: &[String]) -> Vec<&T> {
        todo!()
    }
}
