use futures::{future, TryFutureExt};
use linkerd_app_core::{identity, svc, tls, Conditional, Error};
use std::task::{Context, Poll};
use thiserror::Error;
use tracing::{debug, trace};

const HEADER_NAME: &str = "l5d-require-id";

#[derive(Clone, Debug)]
pub(super) struct NewRequireIdentity<N> {
    inner: N,
}

#[derive(Clone, Debug)]
pub(super) struct RequireIdentity<N> {
    tls: tls::ConditionalClientTls,
    inner: N,
}

#[derive(Debug, Error)]
#[error("required id {required:?}; found {found:?}")]
pub(crate) struct IdentityRequired {
    pub required: tls::client::ServerId,
    pub found: Option<tls::client::ServerId>,
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

    fn new_service(&self, target: T) -> Self::Service {
        let tls = target.param();
        let inner = self.inner.new_service(target);
        RequireIdentity { tls, inner }
    }
}

// === impl RequireIdentity ===

type ResponseFuture<F, T, E> =
    future::Either<future::Ready<Result<T, Error>>, future::MapErr<F, fn(E) -> Error>>;

impl<S> RequireIdentity<S> {
    #[inline]
    fn extract_id<B>(req: &mut http::Request<B>) -> Option<identity::Name> {
        let v = req.headers_mut().remove(HEADER_NAME)?;
        v.to_str().ok()?.parse().ok()
    }
}

impl<S, B> svc::Service<http::Request<B>> for RequireIdentity<S>
where
    S: svc::Service<http::Request<B>>,
    S::Error: Into<Error>,
{
    type Response = S::Response;
    type Error = Error;
    type Future = ResponseFuture<S::Future, S::Response, S::Error>;

    #[inline]
    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx).map_err(Into::into)
    }

    fn call(&mut self, mut request: http::Request<B>) -> Self::Future {
        // If the `l5d-require-id` header is present, then we should expect the target's
        // `peer_identity` to match; if the two values do not match or there is no `peer_identity`,
        // then we fail the request.
        //
        // In either case, we clear the header so it is not passed on outbound requests.
        if let Some(require_id) = Self::extract_id(&mut request) {
            match self.tls.as_ref() {
                Conditional::Some(tls::ClientTls {
                    server_id: tls::ServerId(sni),
                    ..
                }) => {
                    if require_id != *sni {
                        debug!(
                            required = %require_id,
                            found = %sni,
                            "Identity required by header not satisfied"
                        );
                        let e = IdentityRequired {
                            required: require_id.into(),
                            found: Some(tls::ServerId(sni.clone())),
                        };
                        return future::Either::Left(future::err(e.into()));
                    } else {
                        trace!(required = %require_id, "Identity required by header");
                    }
                }
                Conditional::None(_) => {
                    debug!(required = %require_id, "Identity required by header not satisfied");
                    let e = IdentityRequired {
                        required: require_id.into(),
                        found: None,
                    };
                    return future::Either::Left(future::err(e.into()));
                }
            }
        }

        future::Either::Right(self.inner.call(request).map_err(Into::into))
    }
}
