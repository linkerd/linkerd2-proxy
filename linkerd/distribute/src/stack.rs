use crate::{Distribute, Distribution};
use linkerd_stack::{layer, ExtractParam, NewService};
use std::{fmt::Debug, hash::Hash, marker::PhantomData};

/// Builds `Distribute` services for a specific `Distribution`.
#[derive(Clone, Debug)]
pub struct NewDistribute<K, X, N> {
    inner: N,
    extract: X,
    _marker: PhantomData<fn() -> K>,
}

// === impl NewDistribute ===

impl<K, X: Clone, N> NewDistribute<K, X, N> {
    fn new(inner: N, extract: X) -> Self {
        Self {
            inner,
            extract,
            _marker: PhantomData,
        }
    }

    pub fn layer_via(extract: X) -> impl layer::Layer<N, Service = Self> + Clone {
        layer::mk(move |inner| Self::new(inner, extract.clone()))
    }
}

impl<K, N> From<N> for NewDistribute<K, (), N> {
    fn from(inner: N) -> Self {
        Self::new(inner, ())
    }
}

impl<K, N> NewDistribute<K, (), N> {
    pub fn layer() -> impl layer::Layer<N, Service = Self> + Clone {
        layer::mk(Self::from)
    }
}

impl<T, K, KNew, X, N> NewService<T> for NewDistribute<K, X, N>
where
    X: ExtractParam<Distribution<K>, T>,
    K: Debug + Hash + Eq + Clone,
    N: NewService<T, Service = KNew>,
    KNew: NewService<K>,
{
    type Service = Distribute<K, KNew::Service>;

    /// Create a new `Distribute` configured from a `Distribution` param.
    ///
    /// # Panics
    ///
    /// Distributions **MUST** include only keys configured in backends.
    /// Referencing other keys causes a panic.
    fn new_service(&self, target: T) -> Self::Service {
        let dist = self.extract.extract_param(&target);
        tracing::debug!(backends = ?dist, "New distribution");

        let newk = self.inner.new_service(target);
        Distribute::new(dist, |k: &K| newk.new_service(k.clone()))
    }
}
