#![allow(warnings)]

use crate::target::{Concrete, EndpointFromMetadata};
use futures::{future, prelude::*, stream};
use linkerd_app_core::{
    discovery_rejected, is_discovery_rejected,
    proxy::{
        api_resolve::Metadata,
        core::{Resolve, ResolveService, Update},
        discover::{self, Buffer, FromResolve, MakeEndpoint},
        resolve::map_endpoint,
    },
    svc::{
        layer,
        stack::{Filter, Param, Predicate},
        NewService,
    },
    Addr, Error,
};
use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
    time::Duration,
};

type ResolveStack<R> = map_endpoint::Resolve<
    EndpointFromMetadata,
    RecoverDefault<Filter<ResolveService<R>, AllowResolve>>,
>;

fn new_resolve<T, R>(resolve: R) -> ResolveStack<R>
where
    T: Clone + Param<std::net::SocketAddr>,
    EndpointFromMetadata: map_endpoint::MapEndpoint<T, Metadata>,
    R: Resolve<Addr, Endpoint = Metadata>,
    R::Future: Send,
    R::Resolution: Send,
{
    map_endpoint::Resolve::new(
        EndpointFromMetadata,
        RecoverDefault(Filter::new(resolve.into_service(), AllowResolve)),
    )
}

type Stack<E, R, N> = Buffer<discover::Stack<N, ResolveStack<R>, E>>;

pub fn layer<T, E, R, N>(
    resolve: R,
    watchdog: Duration,
) -> impl layer::Layer<N, Service = Stack<E, R, N>> + Clone
where
    T: Clone + Param<std::net::SocketAddr>,
    R: Resolve<Addr, Endpoint = Metadata> + Clone,
    R::Resolution: Send,
    R::Future: Send,
    EndpointFromMetadata: map_endpoint::MapEndpoint<T, Metadata, Out = E>,
    ResolveStack<R>: Resolve<T, Endpoint = E> + Clone,
    N: NewService<E>,
{
    const ENDPOINT_BUFFER_CAPACITY: usize = 1_000;

    let resolve = new_resolve(resolve);
    layer::mk(move |new_endpoint| {
        let endpoints = discover::resolve(new_endpoint, resolve.clone());
        Buffer::new(ENDPOINT_BUFFER_CAPACITY, watchdog, endpoints)
    })
}

#[derive(Clone, Debug)]
pub struct AllowResolve;

/// Wraps a `Resolve` to produce a default resolution when the resolution is
/// rejected.
#[derive(Clone, Debug)]
pub struct RecoverDefault<S>(S);

// === impl AllowResolve ===

impl<P> Predicate<Concrete<P>> for AllowResolve {
    type Request = Addr;

    fn check(&mut self, target: Concrete<P>) -> Result<Addr, Error> {
        target.resolve.ok_or_else(|| discovery_rejected().into())
    }
}

// === impl RecoverDefault ===

type Resolution<R> =
    future::Either<R, stream::Once<future::Ready<Result<Update<Metadata>, Error>>>>;

impl<T, S> tower::Service<T> for RecoverDefault<S>
where
    T: Param<std::net::SocketAddr>,
    S: Resolve<T, Endpoint = Metadata, Error = Error>,
    S::Future: Send + 'static,
    S::Resolution: Send + 'static,
{
    type Response = Resolution<S::Resolution>;
    type Error = Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Error>> + Send + 'static>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Error>> {
        Poll::Ready(futures::ready!(self.0.poll_ready(cx)).map_err(Into::into))
    }

    fn call(&mut self, t: T) -> Self::Future {
        let addr = t.param();
        Box::pin(
            self.0
                .resolve(t)
                .map_ok(future::Either::Left)
                .or_else(move |error| {
                    if is_discovery_rejected(&*error) {
                        tracing::debug!(%error, %addr, "Synthesizing endpoint");
                        let endpoint = (addr, S::Endpoint::default());
                        let res = stream::once(future::ok(Update::Reset(vec![endpoint])));
                        return future::ok(future::Either::Right(res));
                    }

                    future::err(error)
                }),
        )
    }
}
