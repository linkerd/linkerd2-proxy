use super::transport::tls::accept;
use crate::transport::io::BoxedIo;
use crate::Error;
use futures::future::{self, Either, Future, FutureResult};
use futures::Poll;
use linkerd2_proxy_core::listen::Accept;

#[derive(Debug)]
pub struct MissingIdentity;

impl std::fmt::Display for MissingIdentity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "identity is missing")
    }
}

impl std::error::Error for MissingIdentity {}

#[derive(Debug, Clone)]
pub struct RejectWithNoIdentityLayer {
    enabled: bool,
}

#[derive(Debug, Clone)]
pub struct RejectWithNoIdentity<A> {
    enabled: bool,
    accept: A,
}

impl RejectWithNoIdentityLayer {
    pub fn new(enabled: bool) -> Self {
        Self { enabled }
    }
}

impl<A> tower::layer::Layer<A> for RejectWithNoIdentityLayer {
    type Service = RejectWithNoIdentity<A>;

    fn layer(&self, accept: A) -> Self::Service {
        Self::Service {
            accept,
            enabled: self.enabled,
        }
    }
}

impl<A> tower::Service<(accept::Meta, BoxedIo)> for RejectWithNoIdentity<A>
where
    A: Accept<(accept::Meta, BoxedIo)> + Clone,
    A::Error: Into<Error>,
{
    type Response = A::ConnectionFuture;
    type Error = Error;
    type Future = Either<
        FutureResult<Self::Response, Self::Error>,
        future::MapErr<A::Future, fn(A::Error) -> Error>,
    >;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        self.accept.poll_ready().map_err(Into::into)
    }

    fn call(&mut self, (target, io): (accept::Meta, BoxedIo)) -> Self::Future {
        if target.peer_identity.is_none() && self.enabled {
            Either::A(futures::future::err(MissingIdentity.into()))
        } else {
            Either::B(self.accept.accept((target, io)).map_err(Into::into))
        }
    }
}
