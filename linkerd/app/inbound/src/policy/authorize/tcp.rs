use super::super::{AllowPolicy, DeniedUnauthorized, Permit};
use futures::future;
use linkerd_app_core::{
    svc, tls,
    transport::{ClientAddr, Remote},
    Error, Result,
};
use std::{future::Future, pin::Pin, task};

/// A middleware that enforces policy on each HTTP request.
///
/// The inner service is created for each request, so it's expected that this is combined with
#[derive(Clone, Debug)]
pub struct NewAuthorizeTcp<N> {
    inner: N,
}

#[derive(Clone, Debug)]
pub enum AuthorizeTcp<S> {
    Unauthorized(DeniedUnauthorized),
    Authorized {
        inner: S,
        policy: AllowPolicy,
        client: Remote<ClientAddr>,
        tls: tls::ConditionalServerTls,
    },
}

// === impl NewAuthorizeTcp ===

impl<N> NewAuthorizeTcp<N> {
    // FIXME metrics
    pub fn layer() -> impl svc::layer::Layer<N, Service = Self> + Clone {
        svc::layer::mk(|inner| Self { inner })
    }
}

impl<T, N> svc::NewService<T> for NewAuthorizeTcp<N>
where
    T: svc::Param<AllowPolicy>
        + svc::Param<Remote<ClientAddr>>
        + svc::Param<tls::ConditionalServerTls>,
    N: svc::NewService<(Permit, T)>,
{
    type Service = AuthorizeTcp<N::Service>;

    fn new_service(&mut self, target: T) -> Self::Service {
        let client = target.param();
        let tls = target.param();
        let policy: AllowPolicy = target.param();
        match policy.check_authorized(client, &tls) {
            Ok(permit) => {
                tracing::debug!(?permit, "Connection authorized");
                let inner = self.inner.new_service((permit, target));
                AuthorizeTcp::Authorized {
                    inner,
                    policy,
                    client,
                    tls,
                }
            }
            Err(denied) => {
                tracing::info!(?denied, "Connection denied");
                AuthorizeTcp::Unauthorized(denied)
            }
        }
    }
}

// === impl AuthorizeTcp ===

impl<I, S> svc::Service<I> for AuthorizeTcp<S>
where
    S: svc::Service<I, Response = ()>,
    S::Error: Into<Error>,
    S::Future: Send + 'static,
{
    type Response = ();
    type Error = Error;
    type Future = future::Either<
        Pin<Box<dyn Future<Output = Result<()>> + Send + 'static>>,
        future::Ready<Result<()>>,
    >;

    #[inline]
    fn poll_ready(&mut self, cx: &mut task::Context<'_>) -> task::Poll<Result<()>> {
        match self {
            Self::Authorized { ref mut inner, .. } => inner.poll_ready(cx).map_err(Into::into),

            // If connections are not authorized, fail it immediately.
            Self::Unauthorized(deny) => task::Poll::Ready(Err(deny.clone().into())),
        }
    }

    fn call(&mut self, io: I) -> Self::Future {
        match self {
            // If the connection is authorized, pass it to the inner service and stop processing the
            // connection if the authorization's state changes to no longer permit the request.
            Self::Authorized {
                inner,
                client,
                tls,
                policy,
            } => {
                let client = *client;
                let tls = tls.clone();
                let mut policy = policy.clone();

                // FIXME increment counter.

                let call = inner.call(io);
                future::Either::Left(Box::pin(async move {
                    tokio::pin!(call);
                    loop {
                        tokio::select! {
                            res = &mut call => return res.map_err(Into::into),
                            _ = policy.changed() => {
                                if let Err(denied) = policy.check_authorized(client, &tls) {
                                    // FIXME increment counter.
                                    tracing::info!(%denied, "Connection terminated");
                                    return Err(denied.into());
                                }
                            }
                        };
                    }
                }))
            }

            Self::Unauthorized(_deny) => unreachable!("poll_ready must be called"),
        }
    }
}
