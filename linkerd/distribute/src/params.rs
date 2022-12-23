use rand::distributions::WeightedIndex;
use std::sync::Arc;

/// A parameter type that configures how a [`Distribute`] should behave.
#[derive(Clone, Debug, PartialEq)]
pub enum Distribution<K> {
    /// A distribution that has no backends, and therefore never becomes ready.
    Empty,

    /// A distribution that uses the first available backend in an ordered list.
    FirstAvailable(Arc<[K]>),

    /// A distribution that uses the first available backend when randomly
    /// selecting over a weighted distribution of backends.
    RandomAvailable(Arc<WeightedKeys<K>>),
}

#[derive(Debug, PartialEq)]
pub struct WeightedKeys<K> {
    keys: Vec<K>,
    index: WeightedIndex<u32>,
}

// === impl Distribution ===

impl<K> Distribution<K> {
    pub fn first_available(keys: impl IntoIterator<Item = K>) -> Self {
        let keys: Arc<[K]> = keys.into_iter().collect();
        if keys.is_empty() {
            return Self::Empty;
        }
        Self::FirstAvailable(keys)
    }

    pub fn random_available<T: IntoIterator<Item = (K, u32)>>(iter: T) -> Self {
        let (keys, weights): (Vec<_>, Vec<_>) = iter.into_iter().filter(|(_, w)| *w > 0).unzip();
        if keys.len() < 2 {
            return Self::first_available(keys);
        }
        let index = WeightedIndex::new(weights).expect("must succeed");
        Self::RandomAvailable(Arc::new(WeightedKeys { keys, index }))
    }

    pub(crate) fn keys(&self) -> &[K] {
        match self {
            Self::Empty => &[],
            Self::FirstAvailable(keys) => keys,
            Self::RandomAvailable(keys) => keys.keys(),
        }
    }
}

impl<K> From<K> for Distribution<K> {
    fn from(inner: K) -> Self {
        Self::first_available(Some(inner))
    }
}

impl<K> FromIterator<K> for Distribution<K> {
    fn from_iter<T: IntoIterator<Item = K>>(iter: T) -> Self {
        Self::first_available(iter)
    }
}

impl<K> FromIterator<(K, u32)> for Distribution<K> {
    fn from_iter<T: IntoIterator<Item = (K, u32)>>(iter: T) -> Self {
        Self::random_available(iter)
    }
}

// === impl WeightedKeys ===

impl<K> WeightedKeys<K> {
    pub(crate) fn keys(&self) -> &[K] {
        &self.keys
    }

    pub(crate) fn index(&self) -> &WeightedIndex<u32> {
        &self.index
    }
}
