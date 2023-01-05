use ahash::AHashSet;
use rand::distributions::{WeightedError, WeightedIndex};
use std::{fmt::Debug, hash::Hash, sync::Arc};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Backends<K>(pub(crate) Arc<AHashSet<K>>)
where
    K: Eq + Hash + Clone + Debug;

/// A parameter type that configures how a [`Distribute`] should behave.
///
/// [`Distribute`]: crate::service::Distribute
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Distribution<K> {
    /// A distribution that has no backends, and therefore never becomes ready.
    Empty,

    /// A distribution that uses the first available backend in an ordered list.
    FirstAvailable(Arc<[K]>),

    /// A distribution that uses the first available backend when randomly
    /// selecting over a weighted distribution of backends.
    RandomAvailable(Arc<WeightedKeys<K>>),
}

#[derive(Debug, PartialEq, Eq, Hash)]
pub struct WeightedKeys<K> {
    keys: Vec<K>,
    weights: Vec<u32>,
}

// === impl Backends ===

impl<K> From<Arc<AHashSet<K>>> for Backends<K>
where
    K: Eq + Hash + Clone + Debug,
{
    fn from(inner: Arc<AHashSet<K>>) -> Self {
        Self(inner)
    }
}

impl<K> FromIterator<K> for Backends<K>
where
    K: Eq + Hash + Clone + Debug,
{
    fn from_iter<T: IntoIterator<Item = K>>(iter: T) -> Self {
        Self(Arc::new(iter.into_iter().collect()))
    }
}

// === impl Distribution ===

impl<K> From<K> for Distribution<K> {
    fn from(inner: K) -> Self {
        Self::first_available(Some(inner))
    }
}

impl<K> Default for Distribution<K> {
    fn default() -> Self {
        Self::Empty
    }
}

impl<K> Distribution<K> {
    pub fn first_available(keys: impl IntoIterator<Item = K>) -> Self {
        let keys: Arc<[K]> = keys.into_iter().collect();
        if keys.is_empty() {
            return Self::Empty;
        }
        Self::FirstAvailable(keys)
    }

    pub fn random_available<T: IntoIterator<Item = (K, u32)>>(
        iter: T,
    ) -> Result<Self, WeightedError> {
        let (keys, weights): (Vec<_>, Vec<_>) = iter.into_iter().filter(|(_, w)| *w > 0).unzip();
        if keys.len() < 2 {
            return Ok(Self::first_available(keys));
        }
        // Error if the distribution is invalid.
        let _index = WeightedIndex::new(weights.iter().copied())?;
        Ok(Self::RandomAvailable(Arc::new(WeightedKeys {
            keys,
            weights,
        })))
    }

    pub(crate) fn keys(&self) -> &[K] {
        match self {
            Self::Empty => &[],
            Self::FirstAvailable(keys) => keys,
            Self::RandomAvailable(keys) => keys.keys(),
        }
    }
}

// === impl WeightedKeys ===

impl<K> WeightedKeys<K> {
    pub(crate) fn keys(&self) -> &[K] {
        &self.keys
    }

    pub(crate) fn index(&self) -> WeightedIndex<u32> {
        WeightedIndex::new(self.weights.iter().copied()).expect("distribution must be valid")
    }
}
