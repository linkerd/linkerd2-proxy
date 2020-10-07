#![allow(warnings)]

use crate::endpoint;
use futures::{future, prelude::*, stream};
use linkerd2_app_core::{
    proxy::{
        api_resolve::Metadata,
        core::{Resolve, ResolveService, Update},
        discover::{self, Buffer, FromResolve, MakeEndpoint},
        resolve::map_endpoint,
    },
    svc::{
        layer,
        stack::{FilterRequest, RequestFilter},
        NewService,
    },
    Error,
};
use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
    time::Duration,
};

type ResolveStack<F, R> = map_endpoint::Resolve<
    endpoint::FromMetadata,
    RecoverDefault<RequestFilter<F, ResolveService<R>>>,
>;

fn new_resolve<T, F, R>(filter: F, resolve: R) -> ResolveStack<F, R>
where
    T: Clone,
    for<'t> &'t T: Into<std::net::SocketAddr>,
    endpoint::FromMetadata: map_endpoint::MapEndpoint<T, Metadata>,
    F: FilterRequest<T>,
    R: Resolve<F::Request, Endpoint = Metadata>,
{
    map_endpoint::Resolve::new(
        endpoint::FromMetadata,
        RecoverDefault(RequestFilter::new(filter, resolve.into_service())),
    )
}

type Stack<E, F, R, N> = Buffer<discover::Stack<N, ResolveStack<F, R>, E>>;

pub fn new<T, E, F, R, N>(
    filter: F,
    resolve: R,
    new_endpoint: N,
    watchdog: Duration,
) -> Stack<E, F, R, N>
where
    T: Clone,
    for<'t> &'t T: Into<std::net::SocketAddr>,
    F: FilterRequest<T>,
    R: Resolve<F::Request, Error = Error, Endpoint = Metadata>,
    R::Future: Send + 'static,
    R::Resolution: Send + 'static,
    endpoint::FromMetadata: map_endpoint::MapEndpoint<T, Metadata, Out = E>,
    ResolveStack<F, R>: Resolve<T, Endpoint = E>,
    N: NewService<E>,
    //     T: Clone + Send + 'static,
    //     U: Clone + Send + 'static,
    //     endpoint::FromMetadata: map_endpoint::MapEndpoint<T, Metadata, Out = U>,
    //     E: NewService<U> + Clone + Send + Unpin + 'static,
    //     E::Service: Send + 'static,
    //     F: FilterRequest<T> + Clone + Send + Unpin,
    //     R: Resolve<F::Request, Endpoint = Metadata, Error = Error> + Clone + Send + Unpin + 'static,
    //     R::Resolution: stream::TryStream<Ok = Update<Metadata>, Error = Error> + Send + 'static,
    //     R::Future: Send,
    //     RecoverDefault<RequestFilter<F, ResolveService<R>>>:
    //         Resolve<T, Endpoint = Metadata, Error = Error> + Clone,
    //     Stack<E, F, R>: tower::Service<T> + Unpin + Clone + Send,
{
    const ENDPOINT_BUFFER_CAPACITY: usize = 1_000;

    let resolve: ResolveStack<F, R> = new_resolve::<T, F, R>(filter, resolve);
    let endpoints: discover::Stack<N, ResolveStack<F, R>, E> =
        discover::resolve::<T, N, ResolveStack<F, R>>(new_endpoint, resolve);
    Buffer::new(ENDPOINT_BUFFER_CAPACITY, watchdog, endpoints)
}

/// Wraps a `Resolve` to produce a default resolution when the resolution is
/// rejected.
#[derive(Clone, Debug)]
pub struct RecoverDefault<S>(S);

// === impl RecoverDefault ===

type Resolution<R> =
    future::Either<R, stream::Once<future::Ready<Result<Update<Metadata>, Error>>>>;

impl<T, S> tower::Service<T> for RecoverDefault<S>
where
    for<'t> &'t T: Into<std::net::SocketAddr>,
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
        let addr = (&t).into();
        Box::pin(
            self.0
                .resolve(t)
                .map_ok(future::Either::Left)
                .or_else(move |error| {
                    if let Some(status) = error.downcast_ref::<tonic::Status>() {
                        if status.code() == tonic::Code::InvalidArgument {
                            tracing::debug!(%error, %addr, "Synthesizing endpoint");
                            let endpoint = (addr, S::Endpoint::default());
                            let res = stream::once(future::ok(Update::Reset(vec![endpoint])));
                            return future::ok(future::Either::Right(res));
                        }
                    }

                    future::err(error)
                }),
        )
    }
}
