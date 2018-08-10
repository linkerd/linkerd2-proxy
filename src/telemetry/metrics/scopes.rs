use indexmap::IndexMap;
use std::{fmt::Display, hash::Hash};

/// Holds an `S`-typed scope for each `L`-typed label set.
///
/// An `S` type typically holds one or more metrics.
#[derive(Debug)]
pub struct Scopes<L: Display + Hash + Eq, S>(IndexMap<L, S>);

impl<L: Display + Hash + Eq, S> Default for Scopes<L, S> {
    fn default() -> Self {
        Scopes(IndexMap::default())
    }
}

impl<L: Display + Hash + Eq, S> Scopes<L, S> {
    pub fn get(&self, key: &L) -> Option<&S> {
        self.0.get(key)
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn retain<F>(&mut self, f: F)
    where
        F: FnMut(&L, &mut S) -> bool,
    {
        self.0.retain(f)
    }
}

impl<L: Display + Hash + Eq, S: Default> Scopes<L, S> {
    pub fn get_or_default(&mut self, key: L) -> &mut S {
        self.0.entry(key).or_insert_with(|| S::default())
    }
}

impl<'a, L: Display + Hash + Eq, S> IntoIterator for &'a Scopes<L, S> {
    type Item = <&'a IndexMap<L, S> as IntoIterator>::Item;
    type IntoIter = <&'a IndexMap<L, S> as IntoIterator>::IntoIter;

    fn into_iter(self) -> Self::IntoIter {
        self.0.iter()
    }
}
