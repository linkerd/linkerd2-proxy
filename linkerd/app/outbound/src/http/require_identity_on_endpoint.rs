use futures::{
    future::{self, Either},
    TryFutureExt,
};
use linkerd_app_core::{
    errors::IdentityRequired, proxy::http::identity_from_header, svc, tls, Conditional, Error,
    L5D_REQUIRE_ID,
};
use std::task::{Context, Poll};
use tracing::debug;

#[derive(Clone, Debug)]
pub(super) struct NewRequireIdentity<N> {
    inner: N,
}

#[derive(Clone, Debug)]
pub(super) struct RequireIdentity<N> {
    tls: tls::ConditionalClientTls,
    inner: N,
}

// === impl NewRequireIdentity ===

impl<N> NewRequireIdentity<N> {
    fn new(inner: N) -> Self {
        Self { inner }
    }

    pub fn layer() -> impl svc::layer::Layer<N, Service = Self> + Clone + Copy {
        svc::layer::mk(Self::new)
    }
}

impl<T, N> svc::NewService<T> for NewRequireIdentity<N>
where
    T: svc::Param<tls::ConditionalClientTls>,
    N: svc::NewService<T>,
{
    type Service = RequireIdentity<N::Service>;

    fn new_service(&mut self, target: T) -> Self::Service {
        let tls = target.param();
        let inner = self.inner.new_service(target);
        RequireIdentity { tls, inner }
    }
}

// === impl RequireIdentity ===

type ResponseFuture<F, T, E> =
    Either<future::Ready<Result<T, Error>>, future::MapErr<F, fn(E) -> Error>>;

impl<S, A> svc::Service<http::Request<A>> for RequireIdentity<S>
where
    S: svc::Service<http::Request<A>>,
    S::Error: Into<Error>,
{
    type Response = S::Response;
    type Error = Error;
    type Future = ResponseFuture<S::Future, S::Response, S::Error>;

    #[inline]
    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx).map_err(Into::into)
    }

    fn call(&mut self, request: http::Request<A>) -> Self::Future {
        // If the `l5d-require-id` header is present, then we should expect
        // the target's `peer_identity` to match; if the two values do not
        // match or there is no `peer_identity`, then we fail the request
        if let Some(require_id) = identity_from_header(&request, L5D_REQUIRE_ID) {
            debug!("found l5d-require-id={:?}", require_id.as_ref());
            match self.tls.as_ref() {
                Conditional::Some(tls::ClientTls { server_id, .. }) => {
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
