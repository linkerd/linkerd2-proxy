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
    stream::Once<future::Ready<Result<Update<S::Endpoint>, S::Error>>>:
        stream::TryStream<Ok = Update<S::Endpoint>, Error = S::Error>,
{
    type Response = future::Either<
        S::Resolution,
        stream::Once<future::Ready<Result<Update<S::Endpoint>, Error>>>,
    >;
    type Error = Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Error>> + Send + 'static>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.0.poll_ready(cx)
    }

    fn call(&mut self, dst: T) -> Self::Future {
        let addr = (&dst).into();

        Box::pin(
            self.0
                .resolve(dst)
                .map_ok(future::Either::Left)
                .or_else(move |err| {
                    if Rejected::matches(&*err) {
                        tracing::debug!("Handling rejected discovery");

                        let res: stream::Once<
                            future::Ready<Result<Update<S::Endpoint>, S::Error>>,
                        > = stream::once(future::ok(Update::Reset(vec![(
                            addr,
                            S::Endpoint::default(),
                        )])));
                        future::ok(future::Either::Right(res))
                    } else {
                        future::err(err)
                    }
                }),
        )
    }
}
