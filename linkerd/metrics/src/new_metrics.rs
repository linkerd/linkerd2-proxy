use crate::SharedStore;
use linkerd_stack as svc;
use std::{fmt, hash::Hash, marker::PhantomData, sync::Arc};

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

impl<N, K, M, S, T> svc::NewService<T> for NewMetrics<N, K, M, S>
where
    N: svc::NewService<T>,
    S: From<(N::Service, Arc<M>)>,
    M: Default,
    K: Hash + Eq,
    // NOTE(eliza): it would probably be more idiomatic to use `T:
    // Param<SomethingKCanBeBuiltFrom>` here, but that would require this service
    // to be generic over an additional `Param` type. So, just using `From` is
    // probably nicer, since the key type can be the one to declare what `Param`
    // types it can be constructed from.
    K: for<'a> From<&'a T>,
{
    type Service = S;
    fn new_service(&mut self, target: T) -> Self::Service {
        let key = K::from(&target);
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
