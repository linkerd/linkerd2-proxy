use ahash::AHashSet;
use rand::distributions::WeightedError;
use std::{collections::HashMap, fmt::Debug, hash::Hash, sync::Arc};

use crate::{
    keys::{KeyId, UnweightedKeys, WeightedKey},
    WeightedKeys,
};

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
    FirstAvailable(Arc<UnweightedKeys<K>>),

    /// A distribution that uses the first available backend when randomly
    /// selecting over a weighted distribution of backends.
    RandomAvailable(Arc<WeightedKeys<K>>),
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
    pub fn first_available(iter: impl IntoIterator<Item = K>) -> Self {
        let keys = UnweightedKeys::new(iter.into_iter());
        if keys.is_empty() {
            return Self::Empty;
        }

        Self::FirstAvailable(Arc::new(keys))
    }

    pub fn random_available<T: IntoIterator<Item = (K, u32)>>(
        iter: T,
    ) -> Result<Self, WeightedError> {
        let weighted_keys = WeightedKeys::new(
            iter.into_iter()
                .map(|(key, weight)| WeightedKey { key, weight }),
        );
        if weighted_keys.len() < 2 {
            return Ok(Self::FirstAvailable(Arc::new(
                weighted_keys.into_unweighted(),
            )));
        }

        weighted_keys.validate_weights()?;
        Ok(Self::RandomAvailable(Arc::new(weighted_keys)))
    }

    pub(crate) fn make_svc_from_keys<T>(
        &self,
        mut make_svc: impl FnMut(&K) -> T,
    ) -> HashMap<KeyId, T> {
        match self {
            Distribution::Empty => HashMap::new(),
            Distribution::FirstAvailable(keys) => keys
                .iter()
                .map(|&id| (id, make_svc(keys.get(id))))
                .collect(),
            Distribution::RandomAvailable(keys) => keys
                .iter()
                .map(|&id| (id, make_svc(&keys.get(id).key)))
                .collect(),
        }
    }
}
