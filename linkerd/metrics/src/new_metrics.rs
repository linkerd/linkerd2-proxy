use crate::SharedStore;
use linkerd_stack as svc;
use std::{fmt, hash::Hash, marker::PhantomData, sync::Arc};

/// A `NewService` that registers metrics in an inner `SharedStore`.
///
/// Wraps an `N`-typed inner `NewService`, extracting `K`-typed label from each target. The label
/// scope is used to procure an `M`-typed sensor that is used to actually record metrics. The new
/// service uses the inner service and the `M`-typed sensor to construct a new `S`-typed service.
pub struct NewMetrics<N, K: Hash + Eq, M, S> {
    store: SharedStore<K, M>,
    inner: N,
    _svc: PhantomData<fn() -> S>,
}

impl<N, K, M, S> NewMetrics<N, K, M, S>
where
    K: Hash + Eq,
{
    pub fn layer(store: SharedStore<K, M>) -> impl svc::layer::Layer<N, Service = Self> + Clone {
        svc::layer::mk(move |inner| Self {
            store: store.clone(),
            inner,
            _svc: PhantomData,
        })
    }
}

impl<N, K, M, S, T> svc::NewService<T> for NewMetrics<N, K, M, S>
where
    T: svc::Param<K>,
    N: svc::NewService<T>,
    S: From<(N::Service, Arc<M>)>,
    M: Default,
    K: Hash + Eq,
{
    type Service = S;

    fn new_service(&mut self, target: T) -> Self::Service {
        let key = target.param();
        let inner = self.inner.new_service(target);
        let metric = self.store.lock().get_or_default(key).clone();
        S::from((inner, metric))
    }
}

impl<N, K, M, S> Clone for NewMetrics<N, K, M, S>
where
    N: Clone,
    K: Hash + Eq,
{
    fn clone(&self) -> Self {
        Self {
            store: self.store.clone(),
            inner: self.inner.clone(),
            _svc: PhantomData,
        }
    }
}

impl<N, K, M, S> fmt::Debug for NewMetrics<N, K, M, S>
where
    N: fmt::Debug,
    K: Hash + Eq + fmt::Debug,
    M: fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        use std::any::type_name;
        f.debug_struct(type_name::<Self>())
            .field("store", &self.store)
            .field("inner", &self.inner)
            .field("svc", &format_args!("PhantomData<{}>", type_name::<S>()))
            .finish()
    }
}
