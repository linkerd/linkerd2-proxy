use super::{GetRoutes, Receiver};
use futures::{prelude::*, ready};
use linkerd2_error::Error;
use linkerd2_stack::layer;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use tower::{util::ServiceExt, Service};

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
    G: GetRoutes<T>,
    G::Future: Send + 'static,
    G::Error: Send,
    M: Service<(T, Receiver)> + Clone + Send + 'static,
    M::Future: Send + 'static,
    M::Error: Into<Error>,
{
    type Response = M::Response;
    type Error = Error;
    type Future =
        Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send + 'static>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(ready!(self.get_routes.poll_ready(cx)).map_err(Into::into))
    }

    fn call(&mut self, target: T) -> Self::Future {
        let inner = self.inner.clone();
        Box::pin(
            self.get_routes
                .get_routes(target.clone())
                .err_into::<Error>()
                .and_then(move |rx| inner.oneshot((target, rx)).err_into::<Error>()),
        )
    }
}
