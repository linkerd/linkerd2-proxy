use crate::http::Endpoint;
use futures::{
    future::{self, Either},
    TryFutureExt,
};
use linkerd_app_core::{
    errors::IdentityRequired,
    proxy::http::identity_from_header,
    svc::{layer, NewService, Service},
    tls, Conditional, Error, L5D_REQUIRE_ID,
};
use std::task::{Context, Poll};
use tracing::debug;

#[derive(Clone, Debug)]
pub(super) struct NewRequireIdentity<N> {
    inner: N,
}

#[derive(Clone, Debug)]
pub(super) struct RequireIdentity<N> {
    server_id: tls::ConditionalClientTls,
    inner: N,
}

// === impl NewRequireIdentity ===

impl<N> NewRequireIdentity<N> {
    fn new(inner: N) -> Self {
        Self { inner }
    }

    pub fn layer() -> impl layer::Layer<N, Service = Self> + Clone + Copy {
        layer::mk(Self::new)
    }
}

impl<N> NewService<Endpoint> for NewRequireIdentity<N>
where
    N: NewService<Endpoint>,
{
    type Service = RequireIdentity<N::Service>;

    fn new_service(&mut self, target: Endpoint) -> Self::Service {
        let server_id = target.identity.clone();
        let inner = self.inner.new_service(target);
        RequireIdentity { server_id, inner }
    }
}

// === impl RequireIdentity ===

type ResponseFuture<F, T, E> =
    Either<future::Ready<Result<T, Error>>, future::MapErr<F, fn(E) -> Error>>;

impl<S, A> Service<http::Request<A>> for RequireIdentity<S>
where
    S: Service<http::Request<A>>,
    S::Error: Into<Error>,
{
    type Response = S::Response;
    type Error = Error;
    type Future = ResponseFuture<S::Future, S::Response, S::Error>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx).map_err(Into::into)
    }

    fn call(&mut self, request: http::Request<A>) -> Self::Future {
        // If the `l5d-require-id` header is present, then we should expect
        // the target's `peer_identity` to match; if the two values do not
        // match or there is no `peer_identity`, then we fail the request
        if let Some(require_id) = identity_from_header(&request, L5D_REQUIRE_ID) {
            debug!("found l5d-require-id={:?}", require_id.as_ref());
            match self.server_id.as_ref() {
                Conditional::Some(server_id) => {
                    if require_id != *server_id.as_ref() {
                        let e = IdentityRequired {
                            required: require_id.into(),
                            found: Some(server_id.clone()),
                        };
                        return Either::Left(future::err(e.into()));
                    }
                }
                Conditional::None(_) => {
                    let e = IdentityRequired {
                        required: require_id.into(),
                        found: None,
                    };
                    return Either::Left(future::err(e.into()));
                }
            }
        }

        Either::Right(self.inner.call(request).map_err(Into::into))
    }
}
