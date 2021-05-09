use super::{default::RecoverDefault, GetProfile, GetProfileService, Receiver};
use futures::prelude::*;
use linkerd_stack::{layer, Filter, FutureService, NewService, Predicate};
use std::{future::Future, pin::Pin};
use tracing::trace;

type Service<F, G, M> = Discover<RecoverDefault<Filter<GetProfileService<G>, F>>, M>;

pub fn layer<T, G, F, M>(
    get_profile: G,
    filter: F,
) -> impl layer::Layer<M, Service = Service<F, G, M>> + Clone
where
    F: Predicate<T> + Clone,
    G: GetProfile<F::Request> + Clone,
{
    let get_profile = RecoverDefault::new(Filter::new(get_profile.into_service(), filter));
    layer::mk(move |inner| Discover {
        get_profile: get_profile.clone(),
        inner,
    })
}

#[derive(Clone, Debug)]
pub struct Discover<G, M> {
    get_profile: G,
    inner: M,
}

type MakeFuture<S, E> = Pin<Box<dyn Future<Output = Result<S, E>> + Send + 'static>>;

impl<T, G, M> NewService<T> for Discover<G, M>
where
    T: Clone + Send + 'static,
    G: GetProfile<T>,
    G::Future: Send + 'static,
    G::Error: Send,
    M: NewService<(Option<Receiver>, T)> + Clone + Send + 'static,
{
    type Service = FutureService<MakeFuture<M::Service, G::Error>, M::Service>;

    fn new_service(&mut self, target: T) -> Self::Service {
        let mut inner = self.inner.clone();
        let fut = self.get_profile.get_profile(target.clone());
        trace!("Fetching profile");
        FutureService::new(Box::pin(fut.map_ok(move |rx| {
            trace!(has_profile = rx.is_some(), "Building service");
            inner.new_service((rx, target))
        })))
    }
}
