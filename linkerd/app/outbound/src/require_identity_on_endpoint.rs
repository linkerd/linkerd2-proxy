use super::HttpEndpoint;
use futures::{
    future::{self, Either},
    ready, TryFuture, TryFutureExt,
};
use linkerd2_app_core::{
    errors::IdentityRequired,
    proxy::http::identity_from_header,
    svc,
    transport::tls::{self, HasPeerIdentity},
    Conditional, Error, L5D_REQUIRE_ID,
};
use pin_project::pin_project;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use tracing::debug;

#[derive(Clone, Debug, Default)]
pub struct MakeRequireIdentityLayer(());

#[derive(Clone, Debug)]
pub struct MakeRequireIdentity<M> {
    inner: M,
}

#[pin_project]
pub struct MakeFuture<F> {
    peer_identity: tls::PeerIdentity,
    #[pin]
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

impl<M> svc::NewService<HttpEndpoint> for MakeRequireIdentity<M>
where
    M: svc::NewService<HttpEndpoint>,
{
    type Service = RequireIdentity<M::Service>;

    fn new_service(&self, target: HttpEndpoint) -> Self::Service {
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

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, target: T) -> Self::Future {
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
    F: TryFuture,
{
    type Output = Result<RequireIdentity<F::Ok>, F::Error>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();
        let inner = ready!(this.inner.try_poll(cx))?;

        // The inner service is ready and we now create a new service
        // that filters based off `peer_identity` and `l5d-require-id`
        // header
        let svc = RequireIdentity {
            peer_identity: this.peer_identity.clone(),
            inner,
        };

        Poll::Ready(Ok(svc))
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
        future::Ready<Result<Self::Response, Self::Error>>,
        future::MapErr<M::Future, fn(M::Error) -> Error>,
    >;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx).map_err(Into::into)
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
                        return Either::Left(future::err(e.into()));
                    }
                }
                Conditional::None(_) => {
                    let e = IdentityRequired {
                        required: require_identity,
                        found: None,
                    };
                    return Either::Left(future::err(e.into()));
                }
            }
        }

        Either::Right(self.inner.call(request).map_err(Into::into))
    }
}
