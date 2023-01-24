use crate::LookupAddr;

use super::{default::RecoverDefault, GetProfile, Receiver};
use futures::prelude::*;
use linkerd_stack::{layer, FutureService, NewService, Param};
use std::{future::Future, pin::Pin};

#[derive(Clone, Debug)]
pub struct Discover<G, M> {
    get_profile: G,
    inner: M,
}

impl<G: Clone, N> Discover<RecoverDefault<G>, N> {
    pub fn layer(get_profile: G) -> impl layer::Layer<N, Service = Self> + Clone {
        layer::mk(move |inner| Discover {
            get_profile: RecoverDefault::new(get_profile.clone()),
            inner,
        })
    }
}

impl<T, G, N, M> NewService<T> for Discover<G, N>
where
    T: Param<LookupAddr>,
    T: Clone + Send + 'static,
    G: GetProfile + Clone,
    G::Future: Send + 'static,
    G::Error: Send,
    N: NewService<T, Service = M> + Send + 'static,
    M: NewService<Option<Receiver>> + Send + 'static,
{
    type Service = FutureService<
        Pin<Box<dyn Future<Output = Result<M::Service, G::Error>> + Send + 'static>>,
        M::Service,
    >;

    fn new_service(&self, target: T) -> Self::Service {
        let lookup = target.param();
        let inner = self.inner.new_service(target);
        FutureService::new(Box::pin(
            self.get_profile
                .clone()
                .get_profile(lookup)
                .map_ok(move |rx| inner.new_service(rx)),
        ))
    }
}
