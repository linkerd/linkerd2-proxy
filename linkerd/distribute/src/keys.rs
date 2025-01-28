use ahash::{HashMap, HashMapExt};
use rand::{
    distr::weighted::{Error as WeightedError, WeightedIndex},
    prelude::Distribution as _,
    Rng,
};
use std::hash::Hash;

/// Uniquely identifies a key/backend pair for a distribution. This allows
/// backends to have the same key and still participate in request distribution.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct KeyId {
    idx: usize,
}

#[derive(Debug)]
pub struct ServiceKeys<K> {
    ids: Vec<KeyId>,
    keys: HashMap<KeyId, K>,
}

pub type WeightedServiceKeys<K> = ServiceKeys<WeightedKey<K>>;

#[derive(Debug, PartialEq, Eq, Hash)]
pub struct WeightedKey<K> {
    pub key: K,
    pub weight: u32,
}

pub(crate) struct WeightedKeySelector<'a, K> {
    keys: &'a WeightedServiceKeys<K>,
    index: WeightedIndex<u32>,
}

// === impl KeyId ===

impl KeyId {
    pub(crate) fn new(idx: usize) -> Self {
        Self { idx }
    }
}

// === impl UnweightedKeys ===

// PartialEq, Eq, and Hash are all valid to implement for UnweightedKeys since
// there is a defined iteration order for the keys, but it cannot be automatically
// derived for HashMap fields.
impl<K: PartialEq> PartialEq for ServiceKeys<K> {
    fn eq(&self, other: &Self) -> bool {
        if self.ids != other.ids {
            return false;
        }

        for id in &self.ids {
            if self.keys.get(id) != other.keys.get(id) {
                return false;
            }
        }

        true
    }
}

impl<K: Eq> Eq for ServiceKeys<K> {}

impl<K: Hash> Hash for ServiceKeys<K> {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.ids.hash(state);
        // Normally we would also hash the length, but self.ids and
        // self.keys have the same length
        for id in &self.ids {
            self.keys.get(id).hash(state);
        }
    }
}

impl<K> ServiceKeys<K> {
    pub(crate) fn new(iter: impl Iterator<Item = K>) -> Self {
        let mut ids = Vec::new();
        let mut keys = HashMap::new();
        for (idx, key) in iter.enumerate() {
            let id = KeyId::new(idx);
            ids.push(id);
            keys.insert(id, key);
        }

        Self { ids, keys }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.ids.is_empty()
    }

    pub(crate) fn len(&self) -> usize {
        self.ids.len()
    }

    /// Returns the key `K` associated with the given [`KeyId`].
    ///
    /// The output of using a [`KeyId`] not produced by the same instance of
    /// [`ServiceKeys`] is unspecified, and it is likely to panic.
    ///
    /// # Panics
    ///
    /// This will panic if no entry is associated with the given lookup key.
    pub(crate) fn get(&self, id: KeyId) -> &K {
        self.keys
            .get(&id)
            .expect("distribution lookup keys must be valid")
    }

    fn try_get_id(&self, idx: usize) -> Option<KeyId> {
        self.ids.get(idx).copied()
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = &KeyId> {
        self.ids.iter()
    }
}

// === impl WeightedKeys ===

impl<K> WeightedServiceKeys<K> {
    pub(crate) fn into_unweighted(self) -> ServiceKeys<K> {
        ServiceKeys {
            ids: self.ids,
            keys: self
                .keys
                .into_iter()
                .map(|(id, key)| (id, key.key))
                .collect(),
        }
    }

    pub(crate) fn weighted_index(&self) -> Result<WeightedIndex<u32>, WeightedError> {
        WeightedIndex::new(self.ids.iter().map(|&id| self.get(id).weight))
    }

    pub(crate) fn validate_weights(&self) -> Result<(), WeightedError> {
        self.weighted_index()?;
        Ok(())
    }

    pub(crate) fn selector(&self) -> WeightedKeySelector<'_, K> {
        let index = self.weighted_index().expect("distribution must be valid");
        WeightedKeySelector { keys: self, index }
    }
}

// === impl WeightedKeySelector ===

impl<K> WeightedKeySelector<'_, K> {
    pub(crate) fn select_weighted<R: Rng + ?Sized>(&self, rng: &mut R) -> KeyId {
        let idx = self.index.sample(rng);
        self.keys
            .try_get_id(idx)
            .expect("distrubtion must select a valid backend")
    }

    pub(crate) fn disable_backend(&mut self, id: KeyId) -> Result<(), WeightedError> {
        self.index.update_weights(&[(id.idx, &0)])
    }
}
