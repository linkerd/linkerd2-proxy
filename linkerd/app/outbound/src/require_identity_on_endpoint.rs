use super::Endpoint;
use futures::{
    future::{self, Either, FutureResult},
    try_ready, Async, Future, Poll,
};
use linkerd2_app_core::{
    errors::IdentityRequired,
    proxy::http::identity_from_header,
    svc,
    transport::tls::{self, HasPeerIdentity},
    Conditional, Error, L5D_REQUIRE_ID,
};
use tracing::debug;

#[derive(Clone, Debug, Default)]
pub struct MakeRequireIdentityLayer(());

#[derive(Clone, Debug)]
pub struct MakeRequireIdentity<M> {
    inner: M,
}

pub struct MakeFuture<F> {
    peer_identity: tls::PeerIdentity,
    inner: F,
}

#[derive(Clone, Debug)]
pub struct RequireIdentity<M> {
    peer_identity: tls::PeerIdentity,
    inner: M,
}

// === impl MakeRequireIdentityLayer ===

impl MakeRequireIdentityLayer {
    pub fn new() -> Self {
        Self::default()
    }
}

impl<M> svc::Layer<M> for MakeRequireIdentityLayer {
    type Service = MakeRequireIdentity<M>;

    fn layer(&self, inner: M) -> Self::Service {
        MakeRequireIdentity { inner }
    }
}

// === impl MakeRequireIdentity ===

impl<M> svc::NewService<Endpoint> for MakeRequireIdentity<M>
where
    M: svc::NewService<Endpoint>,
{
    type Service = RequireIdentity<M::Service>;

    fn new_service(&self, target: Endpoint) -> Self::Service {
        let peer_identity = target.peer_identity().clone();
        let inner = self.inner.new_service(target);
        RequireIdentity {
            peer_identity,
            inner,
        }
    }
}

impl<T, M> svc::Service<T> for MakeRequireIdentity<M>
where
    T: tls::HasPeerIdentity,
    M: svc::Service<T>,
{
    type Response = RequireIdentity<M::Response>;
    type Error = M::Error;
    type Future = MakeFuture<M::Future>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        self.inner.poll_ready()
    }

    fn call(&mut self, target: T) -> Self::Future {
        // After the inner service is made, we want to wrap that service
        // with a filter that compares the target's `peer_identity` and
        // `l5d_require_id` header if present

        // After the inner service is made, we want to wrap that service
        // with a service that checks for the presence of the
        // `l5d-require-id` header. If is present then assert it is the
        // endpoint identity; otherwise fail the request.
        let peer_identity = target.peer_identity().clone();
        let inner = self.inner.call(target);

        MakeFuture {
            peer_identity,
            inner,
        }
    }
}

impl<F> Future for MakeFuture<F>
where
    F: Future,
{
    type Item = RequireIdentity<F::Item>;
    type Error = F::Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        let inner = try_ready!(self.inner.poll());

        // The inner service is ready and we now create a new service
        // that filters based off `peer_identity` and `l5d-require-id`
        // header
        let svc = RequireIdentity {
            peer_identity: self.peer_identity.clone(),
            inner,
        };

        Ok(Async::Ready(svc))
    }
}

// === impl RequireIdentity ===

impl<M, A> svc::Service<http::Request<A>> for RequireIdentity<M>
where
    M: svc::Service<http::Request<A>>,
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
                        let e = IdentityRequired {
                            required: require_identity,
                            found: Some(peer_identity.clone()),
                        };
                        return Either::A(future::err(e.into()));
                    }
                }
                Conditional::None(_) => {
                    let e = IdentityRequired {
                        required: require_identity,
                        found: None,
                    };
                    return Either::A(future::err(e.into()));
                }
            }
        }

        Either::B(self.inner.call(request).map_err(Into::into))
    }
}
