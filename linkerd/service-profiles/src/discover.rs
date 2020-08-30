use super::{GetProfile, Receiver};
use futures::prelude::*;
use linkerd2_stack::{layer, NewService};
use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};

pub fn layer<G: Clone, M>(get_routes: G) -> impl layer::Layer<M, Service = Discover<G, M>> + Clone {
    layer::mk(move |inner| Discover {
        get_routes: get_routes.clone(),
        inner,
    })
}

#[derive(Clone, Debug)]
pub struct Discover<G, M> {
    get_routes: G,
    inner: M,
}

impl<T, G, M> tower::Service<T> for Discover<G, M>
where
    T: Clone + Send + 'static,
    G: GetProfile<T>,
    G::Future: Send + 'static,
    G::Error: Send,
    M: NewService<(Receiver, T)> + Clone + Send + 'static,
{
    type Response = M::Service;
    type Error = G::Error;
    type Future = Pin<Box<dyn Future<Output = Result<M::Service, G::Error>> + Send + 'static>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.get_routes.poll_ready(cx)
    }

    fn call(&mut self, target: T) -> Self::Future {
        let inner = self.inner.clone();
        Box::pin(
            self.get_routes
                .get_routes(target.clone())
                .map_ok(move |rx| inner.new_service((rx, target))),
        )
    }
}
