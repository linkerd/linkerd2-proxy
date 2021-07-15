use crate::SharedStore;
use linkerd_stack as svc;
use std::{fmt, hash::Hash, marker::PhantomData, sync::Arc};

pub struct NewMetrics<N, K: Hash + Eq, M, S, P = K> {
    store: SharedStore<K, M>,
    inner: N,
    _svc: PhantomData<fn(P) -> S>,
}

impl<N, K, M, S, P> NewMetrics<N, K, M, S, P>
where
    K: Hash + Eq + From<P>,
{
    pub fn layer(store: SharedStore<K, M>) -> impl svc::layer::Layer<N, Service = Self> + Clone {
        svc::layer::mk(move |inner| Self {
            store: store.clone(),
            inner,
            _svc: PhantomData,
        })
    }
}

impl<N, K, M, S, P> fmt::Debug for NewMetrics<N, K, M, S, P>
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
            .field("param", &format_args!("PhantomData<{}>", type_name::<S>()))
            .finish()
    }
}

impl<N, K, M, S, T, P> svc::NewService<T> for NewMetrics<N, K, M, S, P>
where
    T: svc::Param<P>,
    N: svc::NewService<T>,
    S: From<(N::Service, Arc<M>)>,
    M: Default,
    K: Hash + Eq + From<P>,
{
    type Service = S;
    fn new_service(&mut self, target: T) -> Self::Service {
        let key = K::from(target.param());
        let inner = self.inner.new_service(target);
        let metric = self.store.lock().get_or_default(key).clone();
        S::from((inner, metric))
    }
}

impl<N, K, M, S, P> Clone for NewMetrics<N, K, M, S, P>
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
