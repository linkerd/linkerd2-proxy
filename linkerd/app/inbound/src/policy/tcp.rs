use crate::{
    metrics::authz::TcpAuthzMetrics,
    policy::{AllowPolicy, ServerPermit, ServerUnauthorized},
};
use futures::future;
use linkerd_app_core::{
    svc, tls,
    transport::{ClientAddr, OrigDstAddr, Remote},
    Error, Result,
};
use linkerd_proxy_policy::{server, Protocol};
use std::{future::Future, pin::Pin, task};
#[cfg(test)]
mod tests;

/// A middleware that enforces policy on each TCP connection. When connection is
/// authorized, we continue to monitor the policy for changes and, if the
/// connection is no longer authorized, it is dropped/closed.
///
/// Metrics are reported to the `TcpAuthzMetrics` struct.
#[derive(Clone, Debug)]
pub struct NewTcpPolicy<N> {
    inner: N,
    metrics: TcpAuthzMetrics,
}

#[derive(Clone, Debug)]
pub enum TcpPolicy<S> {
    Authorized(Authorized<S>),
    Unauthorized(ServerUnauthorized),
}

#[derive(Clone, Debug)]
pub struct Authorized<S> {
    inner: S,
    policy: AllowPolicy,
    client: Remote<ClientAddr>,
    tls: tls::ConditionalServerTls,
    metrics: TcpAuthzMetrics,
}

// === impl NewTcpPolicy ===

impl<N> NewTcpPolicy<N> {
    pub(crate) fn layer(
        metrics: TcpAuthzMetrics,
    ) -> impl svc::layer::Layer<N, Service = Self> + Clone {
        svc::layer::mk(move |inner| Self {
            inner,
            metrics: metrics.clone(),
        })
    }
}

impl<T, N> svc::NewService<T> for NewTcpPolicy<N>
where
    T: svc::Param<AllowPolicy>
        + svc::Param<Remote<ClientAddr>>
        + svc::Param<tls::ConditionalServerTls>,
    N: svc::NewService<(ServerPermit, T)>,
{
    type Service = TcpPolicy<N::Service>;

    fn new_service(&self, target: T) -> Self::Service {
        let client = target.param();
        let tls = target.param();
        let policy: AllowPolicy = target.param();
        let authorized = {
            let p = policy.server.borrow();
            tracing::trace!(policy = ?p, "Authorizing connection");
            check_authorized(&*p, policy.dst, client, &tls)
        };
        match authorized {
            Ok(permit) => {
                tracing::debug!(?permit, ?tls, %client, "Connection authorized");

                // This new services requires a ClientAddr, so it must necessarily be built for each
                // connection. So we can just increment the counter here since the service can only
                // be used at most once.
                self.metrics.allow(&permit, tls.clone());

                let inner = self.inner.new_service((permit, target));
                TcpPolicy::Authorized(Authorized {
                    inner,
                    policy,
                    client,
                    tls,
                    metrics: self.metrics.clone(),
                })
            }
            Err(deny) => {
                let meta = policy.meta();
                tracing::info!(
                    server.group = %meta.group(),
                    server.kind = %meta.kind(),
                    server.name = %meta.name(),
                    ?tls, %client,
                    "Connection denied"
                );
                self.metrics.deny(&policy, tls);
                TcpPolicy::Unauthorized(deny)
            }
        }
    }
}

// === impl TcpPolicy ===

impl<I, S> svc::Service<I> for TcpPolicy<S>
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
            Self::Authorized(Authorized { ref mut inner, .. }) => {
                inner.poll_ready(cx).map_err(Into::into)
            }

            // If connections are not authorized, fail it immediately.
            Self::Unauthorized(deny) => task::Poll::Ready(Err(deny.clone().into())),
        }
    }

    fn call(&mut self, io: I) -> Self::Future {
        let Authorized {
            inner,
            client,
            tls,
            policy,
            metrics,
        } = match self {
            Self::Authorized(a) => a,
            Self::Unauthorized(_deny) => unreachable!("poll_ready must be called"),
        };

        // If the connection is authorized, pass it to the inner service and stop processing the
        // connection if the authorization's state changes to no longer permit the request.
        let client = *client;
        let tls = tls.clone();
        let mut policy = policy.clone();
        let metrics = metrics.clone();

        let call = inner.call(io);
        future::Either::Left(Box::pin(async move {
            tokio::pin!(call);
            loop {
                tokio::select! {
                    res = &mut call => return res.map_err(Into::into),
                    _ = policy.changed() => {
                        if let Err(denied) = check_authorized(&*policy.server.borrow(), policy.dst, client, &tls) {
                            let meta = policy.meta();
                            tracing::info!(
                                server.group = %meta.group(),
                                server.kind = %meta.kind(),
                                server.name = %meta.name(),
                                ?tls,
                                %client,
                                "Connection terminated due to policy change",
                            );
                            metrics.terminate(&policy, tls);
                            return Err(denied.into());
                        }
                    }
                };
            }
        }))
    }
}

/// Checks whether the destination port's `AllowPolicy` is authorized to
/// accept connections given the provided TLS state.
fn check_authorized(
    server: &server::Policy,
    dst: OrigDstAddr,
    client_addr: Remote<ClientAddr>,
    tls: &tls::ConditionalServerTls,
) -> Result<ServerPermit, ServerUnauthorized> {
    let authzs = match &server.protocol {
        Protocol::Opaque(authzs) => Some(authzs),
        Protocol::Tls(authzs) => Some(authzs),
        // TODO(eliza): if we want to have multiple opaque routes, we would need
        // to actually match them here...punt on that because there's currently
        // only ever one opaque route.
        Protocol::Detect { opaque, .. } => opaque
            .get(0)
            .and_then(|rt| rt.rules.get(0).map(|rule| &rule.policy.authorizations)),
        _ => None,
    };
    for authz in authzs.into_iter().flat_map(|authzs| authzs.iter()) {
        if super::is_authorized(authz, client_addr, tls) {
            return Ok(ServerPermit::new(dst, server, authz));
        }
    }

    Err(ServerUnauthorized {
        server: server.meta.clone(),
    })
}
