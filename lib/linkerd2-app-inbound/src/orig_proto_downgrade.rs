use futures::{Future, Poll};
use http;
use linkerd2_app_core::{proxy::http::orig_proto, svc, transport::Source};
use std::marker::PhantomData;
use tracing::trace;

#[derive(Debug)]
pub struct Layer<A, B>(PhantomData<fn(A) -> B>);

#[derive(Debug)]
pub struct Stack<M, A, B> {
    inner: M,
    _marker: PhantomData<fn(A) -> B>,
}

// === impl Layer ===

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
    M: svc::MakeService<Source, http::Request<A>, Response = http::Response<B>>,
{
    type Service = Stack<M, A, B>;

    fn layer(&self, inner: M) -> Self::Service {
        Stack {
            inner,
            _marker: PhantomData,
        }
    }
}

// === impl Stack ===

impl<M: Clone, A, B> Clone for Stack<M, A, B> {
    fn clone(&self) -> Self {
        Stack {
            inner: self.inner.clone(),
            _marker: PhantomData,
        }
    }
}

impl<M, A, B> svc::Service<Source> for Stack<M, A, B>
where
    M: svc::MakeService<Source, http::Request<A>, Response = http::Response<B>>,
{
    type Response = orig_proto::Downgrade<M::Service>;
    type Error = M::MakeError;
    type Future = futures::future::Map<M::Future, fn(M::Service) -> Self::Response>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        self.inner.poll_ready()
    }

    fn call(&mut self, target: Source) -> Self::Future {
        trace!(
            "supporting {} downgrades for source={:?}",
            orig_proto::L5D_ORIG_PROTO,
            target,
        );
        self.inner
            .make_service(target)
            .map(orig_proto::Downgrade::new)
    }
}
