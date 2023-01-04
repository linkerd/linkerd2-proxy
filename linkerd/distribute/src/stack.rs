use crate::{Distribute, Distribution};
use ahash::AHashMap;
use linkerd_stack::{NewService, Param};
use std::{hash::Hash, sync::Arc};

/// Builds `Distribute` services for a specific `Distribution`.
#[derive(Clone, Debug)]
pub struct NewDistribute<K, S> {
    /// All potential backends.
    all_backends: Arc<AHashMap<K, S>>,
}

// === impl NewDistribute ===

impl<K, S> From<Arc<AHashMap<K, S>>> for NewDistribute<K, S> {
    fn from(all_backends: Arc<AHashMap<K, S>>) -> Self {
        Self { all_backends }
    }
}

impl<K, S> From<AHashMap<K, S>> for NewDistribute<K, S> {
    fn from(backends: AHashMap<K, S>) -> Self {
        Arc::new(backends).into()
    }
}

impl<K: Hash + Eq, S> FromIterator<(K, S)> for NewDistribute<K, S> {
    fn from_iter<T: IntoIterator<Item = (K, S)>>(iter: T) -> Self {
        iter.into_iter().collect::<AHashMap<_, _>>().into()
    }
}

impl<T, K, S> NewService<T> for NewDistribute<K, S>
where
    T: Param<Distribution<K>>,
    K: Hash + Eq + Clone,
    S: Clone,
{
    type Service = Distribute<K, S>;

    /// Create a new `Distribute` configured from a `Distribution` param.
    ///
    /// # Panics
    ///
    /// Distributions **MUST** include only keys configured in backends.
    /// Referencing other keys causes a panic.
    fn new_service(&self, target: T) -> Self::Service {
        let dist = target.param();

        // Clone the backends needed for this distribution, in the required
        // order (so that weighted indices align).
        //
        // If the distribution references a key that is not present in the
        // set of backends, we panic.
        let backends = dist
            .keys()
            .iter()
            .map(|k| {
                let svc = self
                    .all_backends
                    .get(k)
                    .expect("distribution references unknown backend");
                (k.clone(), svc.clone())
            })
            .collect();

        Distribute::new(backends, dist)
    }
}
