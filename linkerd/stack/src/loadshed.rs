use super::layer;
use futures::{future, TryFutureExt};
use linkerd_error::Error;
use std::task::{Context, Poll};
use thiserror::Error;
use tower::Service;

/// A middleware that sheds load when the inner `Service` isn't ready.
#[derive(Clone, Debug)]
pub struct LoadShed<S> {
    inner: S,
    open: bool,
}

/// An error representing that a service is shedding load.
#[derive(Debug, Error)]
#[error("service unavailable")]
pub struct LoadShedError(());

// === impl LoadShed ===

impl<S> LoadShed<S> {
    pub fn layer() -> impl layer::Layer<S, Service = Self> + Copy + Clone {
        layer::mk(|inner| Self { inner, open: true })
    }
}

impl<S, Req> Service<Req> for LoadShed<S>
where
    S: Service<Req>,
    S::Error: Into<Error>,
{
    type Response = S::Response;
    type Error = Error;
    type Future = future::Either<
        future::MapErr<S::Future, fn(S::Error) -> Error>,
        future::Ready<Result<S::Response, Error>>,
    >;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        match self.inner.poll_ready(cx) {
            Poll::Ready(ready) => {
                if !self.open {
                    tracing::debug!("Service has become available");
                }
                self.open = true;
                Poll::Ready(ready.map_err(Into::into))
            }
            Poll::Pending => {
                tracing::debug!("Service unavailable");
                self.open = false;
                Poll::Ready(Ok(()))
            }
        }
    }

    fn call(&mut self, req: Req) -> Self::Future {
        if self.open {
            future::Either::Left(self.inner.call(req).map_err(Into::into))
        } else {
            future::Either::Right(future::err(LoadShedError(()).into()))
        }
    }
}
