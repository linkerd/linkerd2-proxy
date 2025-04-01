use crate::SharedStore;
use linkerd_stack as svc;
use std::{fmt, hash::Hash, marker::PhantomData, sync::Arc};

/// A `NewService` that registers metrics in an inner `SharedStore`.
///
/// Wraps an `N`-typed inner `NewService`, extracting `K`-typed label from each target. The label
/// scope is used to procure an `M`-typed sensor that is used to actually record metrics. The new
/// service uses the inner service and the `M`-typed sensor to construct a new `S`-typed service.
pub struct NewMetrics<N, K: Hash + Eq, X, M, S> {
    store: SharedStore<K, M>,
    inner: N,
    params: X,
    _svc: PhantomData<fn() -> S>,
}

impl<N, K, X, M, S> NewMetrics<N, K, X, M, S>
where
    K: Hash + Eq,
    X: Clone,
{
    pub fn layer_via(
        store: SharedStore<K, M>,
        params: X,
    ) -> impl svc::layer::Layer<N, Service = Self> + Clone {
        svc::layer::mk(move |inner| Self {
            store: store.clone(),
            inner,
            params: params.clone(),
            _svc: PhantomData,
        })
    }
}

impl<N, K, M, S> NewMetrics<N, K, (), M, S>
where
    K: Hash + Eq,
{
    pub fn layer(store: SharedStore<K, M>) -> impl svc::layer::Layer<N, Service = Self> + Clone {
        Self::layer_via(store, ())
    }
}

impl<N, K, X, M, S, T> svc::NewService<T> for NewMetrics<N, K, X, M, S>
where
    N: svc::NewService<T>,
    S: From<(N::Service, Arc<M>)>,
    M: Default,
    K: Hash + Eq,
    X: svc::ExtractParam<K, T>,
{
    type Service = S;

    fn new_service(&self, target: T) -> Self::Service {
        let key = self.params.extract_param(&target);
        let inner = self.inner.new_service(target);
        let metric = self.store.lock().get_or_default(key).clone();
        S::from((inner, metric))
    }
}

impl<N, K, X, M, S> Clone for NewMetrics<N, K, X, M, S>
where
    N: Clone,
    K: Hash + Eq,
    X: Clone,
{
    fn clone(&self) -> Self {
        Self {
            store: self.store.clone(),
            inner: self.inner.clone(),
            params: self.params.clone(),
            _svc: PhantomData,
        }
    }
}

impl<N, K, X, M, S> fmt::Debug for NewMetrics<N, K, X, M, S>
where
    N: fmt::Debug,
    K: Hash + Eq + fmt::Debug,
    X: fmt::Debug,
    M: fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        use std::any::type_name;
        f.debug_struct(type_name::<Self>())
            .field("store", &self.store)
            .field("params", &self.params)
            .field("inner", &self.inner)
            .field("svc", &format_args!("PhantomData<{}>", type_name::<S>()))
            .finish()
    }
}
