use super::{default::RecoverDefault, GetProfile, Receiver};
use futures::prelude::*;
use linkerd2_stack::{layer, FutureService, NewService};
use std::{future::Future, pin::Pin};

pub fn layer<G: Clone, M>(
    get_profile: G,
) -> impl layer::Layer<M, Service = Discover<RecoverDefault<G>, M>> + Clone {
    layer::mk(move |inner| Discover {
        get_profile: RecoverDefault::new(get_profile.clone()),
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
