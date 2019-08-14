use super::Endpoint;
use crate::proxy::http::{orig_proto, settings::Settings};
use crate::svc;
use futures::{try_ready, Future, Poll};
use http;
use std::marker::PhantomData;
use tracing::trace;

#[derive(Debug)]
pub struct Layer<A, B>(PhantomData<fn(A) -> B>);

#[derive(Debug)]
pub struct MakeSvc<M, A, B> {
    inner: M,
    _marker: PhantomData<fn(A) -> B>,
}

pub struct MakeFuture<F, A, B> {
    can_upgrade: bool,
    inner: F,
    _marker: PhantomData<fn(A) -> B>,
}

pub fn layer<A, B>() -> Layer<A, B> {
    Layer(PhantomData)
}

impl<A, B> Clone for Layer<A, B> {
    fn clone(&self) -> Self {
        Layer(PhantomData)
    }
}

impl<M, A, B> svc::Layer<M> for Layer<A, B>
where
    M: svc::MakeService<Endpoint, http::Request<A>, Response = http::Response<B>>,
{
    type Service = MakeSvc<M, A, B>;

    fn layer(&self, inner: M) -> Self::Service {
        MakeSvc {
            inner,
            _marker: PhantomData,
        }
    }
}

// === impl MakeSvc ===

impl<M: Clone, A, B> Clone for MakeSvc<M, A, B> {
    fn clone(&self) -> Self {
        MakeSvc {
            inner: self.inner.clone(),
            _marker: PhantomData,
        }
    }
}

impl<M, A, B> svc::Service<Endpoint> for MakeSvc<M, A, B>
where
    M: svc::MakeService<Endpoint, http::Request<A>, Response = http::Response<B>>,
{
    type Response = svc::Either<orig_proto::Upgrade<M::Service>, M::Service>;
    type Error = M::MakeError;
    type Future = MakeFuture<M::Future, A, B>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        self.inner.poll_ready()
    }

    fn call(&mut self, mut endpoint: Endpoint) -> Self::Future {
        let can_upgrade = endpoint.can_use_orig_proto();

        if can_upgrade {
            trace!(
                "supporting {} upgrades for endpoint={:?}",
                orig_proto::L5D_ORIG_PROTO,
                endpoint,
            );
            endpoint.http_settings = Settings::Http2;
        }

        let inner = self.inner.make_service(endpoint);
        MakeFuture {
            can_upgrade,
            inner,
            _marker: PhantomData,
        }
    }
}

// === impl MakeFuture ===

impl<F, A, B> Future for MakeFuture<F, A, B>
where
    F: Future,
    F::Item: svc::Service<http::Request<A>, Response = http::Response<B>>,
{
    type Item = svc::Either<orig_proto::Upgrade<F::Item>, F::Item>;
    type Error = F::Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        let inner = try_ready!(self.inner.poll());

        if self.can_upgrade {
            Ok(svc::Either::A(orig_proto::Upgrade::new(inner)).into())
        } else {
            Ok(svc::Either::B(inner).into())
        }
    }
}
