use super::Endpoint;
use futures::{
    future::{self, Either, FutureResult},
    try_ready, Async, Future, Poll,
};
use linkerd2_app_core::{
    errors,
    proxy::http::identity_from_header,
    svc,
    transport::tls::{self, HasPeerIdentity},
    Conditional, Error, L5D_REQUIRE_ID,
};
use std::marker::PhantomData;
use tracing::debug;

pub struct Layer<A, B>(PhantomData<fn(A) -> B>);

pub struct MakeSvc<M, A, B> {
    inner: M,
    _marker: PhantomData<fn(A) -> B>,
}

pub struct MakeFuture<F, A, B> {
    peer_identity: tls::PeerIdentity,
    inner: F,
    _marker: PhantomData<fn(A) -> B>,
}

pub struct RequireIdentity<M, A, B> {
    peer_identity: tls::PeerIdentity,
    inner: M,
    _marker: PhantomData<fn(A) -> B>,
}

// ===== impl Layer =====

pub fn layer<A, B>() -> Layer<A, B> {
    Layer(PhantomData)
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

impl<A, B> Clone for Layer<A, B> {
    fn clone(&self) -> Self {
        Layer(PhantomData)
    }
}

// ===== impl MakeSvc =====

impl<M, A, B> svc::Service<Endpoint> for MakeSvc<M, A, B>
where
    M: svc::MakeService<Endpoint, http::Request<A>, Response = http::Response<B>>,
{
    type Response = RequireIdentity<M::Service, A, B>;
    type Error = M::MakeError;
    type Future = MakeFuture<M::Future, A, B>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        self.inner.poll_ready()
    }

    fn call(&mut self, target: Endpoint) -> Self::Future {
        // After the inner service is made, we want to wrap that service
        // with a filter that compares the target's `peer_identity` and
        // `l5d_require_id` header if present

        // After the inner service is made, we want to wrap that service
        // with a service that checks for the presence of the
        // `l5d-require-id` header. If is present then assert it is the
        // endpoint identity; otherwise fail the request.
        let peer_identity = target.peer_identity().clone();
        let inner = self.inner.make_service(target);

        MakeFuture {
            peer_identity,
            inner,
            _marker: PhantomData,
        }
    }
}

impl<F, A, B> Future for MakeFuture<F, A, B>
where
    F: Future,
    F::Item: svc::Service<http::Request<A>, Response = http::Response<B>>,
{
    type Item = RequireIdentity<F::Item, A, B>;
    type Error = F::Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        let inner = try_ready!(self.inner.poll());

        // The inner service is ready and we now create a new service
        // that filters based off `peer_identity` and `l5d-require-id`
        // header
        let svc = RequireIdentity {
            peer_identity: self.peer_identity.clone(),
            inner,
            _marker: PhantomData,
        };

        Ok(Async::Ready(svc))
    }
}

impl<M: Clone, A, B> Clone for MakeSvc<M, A, B> {
    fn clone(&self) -> Self {
        MakeSvc {
            inner: self.inner.clone(),
            _marker: PhantomData,
        }
    }
}

// ===== impl RequireIdentity =====

impl<M, A, B> svc::Service<http::Request<A>> for RequireIdentity<M, A, B>
where
    M: svc::Service<http::Request<A>, Response = http::Response<B>>,
    M::Error: Into<Error>,
{
    type Response = M::Response;
    type Error = Error;
    type Future = Either<
        FutureResult<Self::Response, Self::Error>,
        future::MapErr<M::Future, fn(M::Error) -> Error>,
    >;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        self.inner.poll_ready().map_err(Into::into)
    }

    fn call(&mut self, request: http::Request<A>) -> Self::Future {
        // If the `l5d-require-id` header is present, then we should expect
        // the target's `peer_identity` to match; if the two values do not
        // match or there is no `peer_identity`, then we fail the request
        if let Some(require_identity) = identity_from_header(&request, L5D_REQUIRE_ID) {
            debug!("found l5d-require-id={:?}", require_identity.as_ref());
            match self.peer_identity {
                Conditional::Some(ref peer_identity) => {
                    if require_identity != *peer_identity {
                        let message = format!(
                            "require identity check failed; require={:?} found={:?}",
                            require_identity, peer_identity
                        );
                        let e = errors::StatusError {
                            message,
                            status: http::StatusCode::FORBIDDEN,
                        };
                        return Either::A(future::err(e.into()));
                    }
                }
                Conditional::None(_) => {
                    let message =
                        "require identity check failed; no peer_identity found".to_string();
                    let e = errors::StatusError {
                        message,
                        status: http::StatusCode::FORBIDDEN,
                    };
                    return Either::A(future::err(e.into()));
                }
            }
        }

        Either::B(self.inner.call(request).map_err(Into::into))
    }
}
