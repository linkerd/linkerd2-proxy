use crate::http::Endpoint;
use futures::{
    future::{self, Either},
    TryFutureExt,
};
use linkerd2_app_core::{
    errors::IdentityRequired,
    proxy::http::identity_from_header,
    svc,
    transport::tls::{self, HasPeerIdentity},
    Conditional, Error, L5D_REQUIRE_ID,
};
use std::task::{Context, Poll};
use tracing::debug;

#[derive(Clone, Debug)]
pub(super) struct NewRequireIdentity<N> {
    inner: N,
}

#[derive(Clone, Debug)]
pub(super) struct RequireIdentity<N> {
    peer_identity: tls::PeerIdentity,
    inner: N,
}

// === impl NewRequireIdentity ===

impl<N> NewRequireIdentity<N>
where
    N: svc::NewService<Endpoint>,
{
    pub fn new(inner: N) -> Self {
        Self { inner }
    }
}

impl<N> svc::NewService<Endpoint> for NewRequireIdentity<N>
where
    N: svc::NewService<Endpoint>,
{
    type Service = RequireIdentity<N::Service>;

    fn new_service(&mut self, target: Endpoint) -> Self::Service {
        let peer_identity = target.peer_identity().clone();
        let inner = self.inner.new_service(target);
        RequireIdentity {
            peer_identity,
            inner,
        }
    }
}

// === impl RequireIdentity ===

impl<S, A> svc::Service<http::Request<A>> for RequireIdentity<S>
where
    S: svc::Service<http::Request<A>>,
    S::Error: Into<Error>,
{
    type Response = S::Response;
    type Error = Error;
    type Future = Either<
        future::Ready<Result<Self::Response, Self::Error>>,
        future::MapErr<S::Future, fn(S::Error) -> Error>,
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
