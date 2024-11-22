use linkerd_app_core::{io, svc, Error};
use linkerd_proxy_client_policy::tls;
use std::{
    fmt::Debug,
    future::Future,
    pin::Pin,
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
    filters: Arc<[tls::Filter]>,
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
    T: svc::Param<Arc<[tls::Filter]>>,
{
    type Service = ApplyFilters<S>;

    fn new_service(&self, target: T) -> Self::Service {
        let filters: Arc<[tls::Filter]> = target.param();
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
                tls::Filter::Forbidden => {
                    return Err(errors::TLSForbiddenRoute.into());
                }

                tls::Filter::Invalid(message) => {
                    return Err(errors::TLSInvalidBackend(message.clone()).into());
                }

                tls::Filter::InternalError(message) => {
                    return Err(errors::TLSInvalidPolicy(message).into());
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
    type Future = Pin<Box<dyn Future<Output = Result<S::Response, Error>> + Send + 'static>>;

    #[inline]
    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx).map_err(Into::into)
    }

    fn call(&mut self, io: I) -> Self::Future {
        let call = self.inner.call(io);
        let apply = self.apply_filters();

        Box::pin(async move {
            apply?;
            call.await.map_err(Into::into)
        })
    }
}

pub mod errors {
    use super::*;

    #[derive(Debug, thiserror::Error)]
    #[error("forbidden TLS route")]
    pub struct TLSForbiddenRoute;

    #[derive(Debug, thiserror::Error)]
    #[error("invalid TLS backend: {0}")]
    pub struct TLSInvalidBackend(pub Arc<str>);

    #[derive(Debug, thiserror::Error)]
    #[error("invalid client policy: {0}")]
    pub struct TLSInvalidPolicy(pub &'static str);
}
