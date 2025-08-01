use super::*;
use std::{
    collections::{HashMap, HashSet, hash_map},
    sync::Arc,
};

mod ipn_pattern_map;
mod pattern_map;

#[cfg(feature = "dtn-pat-item")]
mod dtn_pattern_map;

#[derive(Debug)]
pub struct EidPatternMap<V: Eq + std::hash::Hash>(pattern_map::PatternMap<V>);

impl<V: Eq + std::hash::Hash> EidPatternMap<V> {
    pub fn new() -> Self {
        Default::default()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn clear(&mut self) {
        self.0 = Default::default()
    }

    pub fn insert(&mut self, pattern: EidPattern, value: V) {
        self.0.insert(pattern, value)
    }

    pub fn remove<B>(&mut self, pattern: &EidPattern) -> B
    where
        B: FromIterator<V>,
    {
        self.0.remove(pattern).collect()
    }

    pub fn remove_if<B, F>(&mut self, pattern: &EidPattern, f: F) -> B
    where
        B: FromIterator<V>,
        F: Fn(&V) -> bool,
    {
        self.0.remove_if(pattern, f).collect()
    }

    pub fn contains(&self, eid: &Eid) -> bool {
        self.0.find(eid).next().is_some()
    }

    pub fn find<'a, B>(&'a self, eid: &Eid) -> B
    where
        B: FromIterator<&'a V>,
    {
        self.0.find(eid).collect()
    }
}

impl<V: Eq + std::hash::Hash> Default for EidPatternMap<V> {
    fn default() -> Self {
        Self(Default::default())
    }
}

#[derive(Debug, Default)]
pub struct EidPatternSet(pattern_map::PatternMap<()>);

impl EidPatternSet {
    pub fn new() -> Self {
        Default::default()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn clear(&mut self) {
        self.0 = Default::default()
    }

    pub fn insert(&mut self, pattern: EidPattern) {
        self.0.insert(pattern, ())
    }

    pub fn remove(&mut self, pattern: &EidPattern) -> bool {
        self.0.remove(pattern).next().is_some()
    }

    pub fn contains(&self, eid: &Eid) -> bool {
        self.0.find(eid).next().is_some()
    }
}

impl From<EidPattern> for EidPatternSet {
    fn from(value: EidPattern) -> Self {
        let mut s = Self::new();
        s.insert(value);
        s
    }
}
