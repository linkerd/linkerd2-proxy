use futures::{future, TryFutureExt};
use linkerd_app_core::{io, svc, Error};
use linkerd_proxy_client_policy::opaq;
use std::{
    fmt::Debug,
    sync::Arc,
    task::{Context, Poll},
};

#[derive(Clone, Debug)]
pub struct NewApplyFilters<N> {
    inner: N,
}

#[derive(Clone, Debug)]
pub struct ApplyFilters<S> {
    inner: S,
    filters: Arc<[opaq::Filter]>,
}

// === impl NewApplyFilters ===

impl<N> NewApplyFilters<N> {
    pub fn layer() -> impl svc::layer::Layer<N, Service = Self> + Clone {
        svc::layer::mk(move |inner| Self { inner })
    }
}

impl<T, N, S> svc::NewService<T> for NewApplyFilters<N>
where
    N: svc::NewService<T, Service = S>,
    T: svc::Param<Arc<[opaq::Filter]>>,
{
    type Service = ApplyFilters<S>;

    fn new_service(&self, target: T) -> Self::Service {
        let filters: Arc<[opaq::Filter]> = target.param();
        let svc = self.inner.new_service(target);
        ApplyFilters {
            inner: svc,
            filters,
        }
    }
}

// === impl ApplyFilters ===

impl<S> ApplyFilters<S> {
    fn apply_filters(&self) -> Result<(), Error> {
        if let Some(filter) = self.filters.iter().next() {
            match filter {
                opaq::Filter::Forbidden => {
                    return Err(errors::TCPForbiddenRoute.into());
                }

                opaq::Filter::Invalid(message) => {
                    return Err(errors::TCPInvalidBackend(message.clone()).into());
                }

                opaq::Filter::InternalError(message) => {
                    return Err(errors::TCPInvalidPolicy(message).into());
                }
            }
        }

        Ok(())
    }
}

impl<I, S> svc::Service<I> for ApplyFilters<S>
where
    I: io::AsyncRead + io::AsyncWrite + Send + 'static,
    S: svc::Service<I> + Send + Clone + 'static,
    S::Error: Into<Error>,
    S::Future: Send,
{
    type Response = S::Response;
    type Error = Error;
    type Future = future::Either<
        future::ErrInto<S::Future, Error>,
        future::Ready<Result<S::Response, Error>>,
    >;

    #[inline]
    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx).map_err(Into::into)
    }

    fn call(&mut self, io: I) -> Self::Future {
        if let Err(e) = self.apply_filters() {
            return future::Either::Right(future::err(e));
        }
        future::Either::Left(self.inner.call(io).err_into())
    }
}

pub mod errors {
    use super::*;

    #[derive(Debug, thiserror::Error)]
    #[error("forbidden TCP route")]
    pub struct TCPForbiddenRoute;

    #[derive(Debug, thiserror::Error)]
    #[error("invalid TCP backend: {0}")]
    pub struct TCPInvalidBackend(pub Arc<str>);

    #[derive(Debug, thiserror::Error)]
    #[error("invalid client policy: {0}")]
    pub struct TCPInvalidPolicy(pub &'static str);
}
