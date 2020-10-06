use super::{default::RecoverDefault, GetProfile, GetProfileService, Receiver};
use futures::prelude::*;
use linkerd2_stack::{layer, FilterRequest, FutureService, NewService, RequestFilter};
use std::{future::Future, pin::Pin};

type Service<F, G, M> = Discover<RecoverDefault<RequestFilter<F, GetProfileService<G>>>, M>;

pub fn layer<T, G, F, M>(
    get_profile: G,
    filter: F,
) -> impl layer::Layer<M, Service = Service<F, G, M>> + Clone
where
    F: FilterRequest<T> + Clone,
    G: GetProfile<F::Request> + Clone,
{
    let get_profile = RecoverDefault::new(RequestFilter::new(filter, get_profile.into_service()));
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

impl<T, G, M> NewService<T> for Discover<G, M>
where
    T: Clone + Send + 'static,
    G: GetProfile<T>,
    G::Future: Send + 'static,
    G::Error: Send,
    M: NewService<(Option<Receiver>, T)> + Clone + Send + 'static,
{
    type Service = FutureService<
        Pin<Box<dyn Future<Output = Result<M::Service, G::Error>> + Send + 'static>>,
        M::Service,
    >;

    fn new_service(&mut self, target: T) -> Self::Service {
        let mut inner = self.inner.clone();
        FutureService::new(Box::pin(
            self.get_profile
                .get_profile(target.clone())
                .map_ok(move |rx| inner.new_service((rx, target))),
        ))
    }
}
