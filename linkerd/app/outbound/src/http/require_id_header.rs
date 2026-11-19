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

#[derive(Debug, Error)]
#[error("invalid l5d-require-id header: {0:?}")]
pub(crate) struct InvalidRequiredIdentity(http::HeaderValue);

// === impl NewRequireIdentity ===

impl<N> NewRequireIdentity<N> {
    fn new(inner: N) -> Self {
        Self { inner }
    }

    pub fn layer() -> impl svc::layer::Layer<N, Service = Self> + Copy {
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
    fn extract_id<B>(
        req: &mut http::Request<B>,
    ) -> Result<Option<identity::Id>, InvalidRequiredIdentity> {
        let Some(v) = req.headers_mut().remove(HEADER_NAME) else {
            return Ok(None);
        };
        match v.to_str().ok().and_then(|s| s.parse().ok()) {
            Some(id) => Ok(Some(id)),
            None => Err(InvalidRequiredIdentity(v)),
        }
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
        //
        // A header that cannot be parsed as an identity can never be satisfied,
        // so the request fails rather than being sent without the check.
        let require_id = match Self::extract_id(&mut request) {
            Ok(id) => id,
            Err(e) => {
                debug!(error = %e, "Identity required by header not satisfied");
                return future::Either::Left(future::err(e.into()));
            }
        };
        if let Some(require_id) = require_id {
            match self.tls.as_ref() {
                Conditional::Some(tls::ClientTls { server_id, .. }) => {
                    if require_id != **server_id {
                        debug!(
                            required = %require_id,
                            found = %server_id,
                            "Identity required by header not satisfied"
                        );
                        let e = IdentityRequired {
                            required: require_id.into(),
                            found: Some(server_id.clone()),
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

#[cfg(test)]
mod tests {
    use super::*;
    use linkerd_app_core::svc::Service;

    const SERVER_ID: &str = "foo.ns1.serviceaccount.identity.linkerd.cluster.local";

    fn mk(
        tls: tls::ConditionalClientTls,
    ) -> RequireIdentity<impl Service<http::Request<()>, Response = (), Error = Error>> {
        let inner = svc::mk(|_: http::Request<()>| future::ok::<(), Error>(()));
        RequireIdentity { tls, inner }
    }

    fn mtls() -> tls::ConditionalClientTls {
        Conditional::Some(tls::ClientTls::new(
            tls::client::ServerId(SERVER_ID.parse().unwrap()),
            tls::ServerName(SERVER_ID.parse().unwrap()),
        ))
    }

    fn req(require_id: Option<&str>) -> http::Request<()> {
        let mut req = http::Request::builder();
        if let Some(id) = require_id {
            req = req.header(HEADER_NAME, id);
        }
        req.body(()).unwrap()
    }

    #[tokio::test]
    async fn matching_identity() {
        let mut svc = mk(mtls());
        assert!(svc.call(req(None)).await.is_ok());
        assert!(svc.call(req(Some(SERVER_ID))).await.is_ok());
    }

    #[tokio::test]
    async fn mismatched_identity() {
        let mut svc = mk(mtls());
        let err = svc
            .call(req(Some(
                "bar.ns1.serviceaccount.identity.linkerd.cluster.local",
            )))
            .await
            .unwrap_err();
        assert!(err.is::<IdentityRequired>());

        let mut svc = mk(Conditional::None(
            tls::NoClientTls::NotProvidedByServiceDiscovery,
        ));
        let err = svc.call(req(Some(SERVER_ID))).await.unwrap_err();
        assert!(err.is::<IdentityRequired>());
    }

    #[tokio::test]
    async fn invalid_identity() {
        for tls in [
            mtls(),
            Conditional::None(tls::NoClientTls::NotProvidedByServiceDiscovery),
        ] {
            let mut svc = mk(tls);
            for id in [
                "",
                "foo.ns1.serviceaccount.identity.linkerd.cluster.local.",
                "not an identity",
            ] {
                let err = svc.call(req(Some(id))).await.unwrap_err();
                assert!(
                    err.is::<InvalidRequiredIdentity>(),
                    "{id:?} must be rejected"
                );
            }
        }
    }
}
