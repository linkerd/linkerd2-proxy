use super::Rejected;
use futures::{future, prelude::*, stream};
use linkerd2_app_core::{
    proxy::core::{Resolve, Update},
    svc, Error,
};
use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};

pub fn layer<S>() -> impl svc::Layer<S, Service = RecoverDefaultResolve<S>> + Clone {
    svc::layer::mk(RecoverDefaultResolve)
}

#[derive(Clone, Debug)]
pub struct RecoverDefaultResolve<S>(S);

impl<T, S> tower::Service<T> for RecoverDefaultResolve<S>
where
    for<'t> &'t T: Into<std::net::SocketAddr>,
    S: Resolve<T, Error = Error>,
    S::Endpoint: Default + Send + 'static,
    S::Resolution: Send + 'static,
    S::Future: Send + 'static,
    stream::Chain<
        stream::Once<future::Ready<Result<Update<S::Endpoint>, Error>>>,
        stream::Pending<Result<Update<S::Endpoint>, Error>>,
    >: stream::TryStream<Ok = Update<S::Endpoint>, Error = S::Error>,
{
    type Response = future::Either<
        S::Resolution,
        stream::Chain<
            stream::Once<future::Ready<Result<Update<S::Endpoint>, Error>>>,
            stream::Pending<Result<Update<S::Endpoint>, Error>>,
        >,
    >;
    type Error = Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Error>> + Send + 'static>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.0.poll_ready(cx)
    }

    fn call(&mut self, t: T) -> Self::Future {
        let addr = (&t).into();
        Box::pin(
            self.0
                .resolve(t)
                .map_ok(future::Either::Left)
                .or_else(move |error| {
                    if Rejected::matches(&*error) {
                        tracing::debug!(%error, %addr, "Synthesizing endpoint");
                        let endpoint = (addr, S::Endpoint::default());
                        let res = stream::once(future::ok(Update::Reset(vec![endpoint])))
                            .chain(stream::pending());
                        future::ok(future::Either::Right(res))
                    } else {
                        future::err(error)
                    }
                }),
        )
    }
}
