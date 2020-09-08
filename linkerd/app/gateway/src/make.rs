use super::gateway::Gateway;
use futures::{future, ready};
use linkerd2_app_core::{dns, proxy::identity, transport::tls, Error, NameAddr};
use linkerd2_app_inbound::endpoint as inbound;
use linkerd2_app_outbound::endpoint as outbound;
use std::future::Future;
use std::net::IpAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

#[derive(Clone, Debug)]
pub(crate) enum MakeGateway<R, O> {
    NoIdentity,
    NoSuffixes,
    Enabled {
        resolve: R,
        outbound: O,
        suffixes: Arc<Vec<dns::Suffix>>,
        local_identity: identity::Name,
    },
}

impl<R, O> MakeGateway<R, O> {
    pub fn new(
        resolve: R,
        outbound: O,
        local_identity: tls::PeerIdentity,
        suffixes: impl IntoIterator<Item = dns::Suffix>,
    ) -> Self {
        let suffixes = match suffixes.into_iter().collect::<Vec<_>>() {
            s if s.is_empty() => return Self::NoSuffixes,
            s => Arc::new(s),
        };
        let local_identity = match local_identity {
            tls::Conditional::None(_) => return Self::NoIdentity,
            tls::Conditional::Some(id) => id,
        };
        MakeGateway::Enabled {
            resolve,
            outbound,
            local_identity,
            suffixes,
        }
    }
}

impl<R, O> tower::Service<inbound::Target> for MakeGateway<R, O>
where
    R: tower::Service<dns::Name, Response = (dns::Name, IpAddr)>,
    R::Error: Into<Error> + 'static,
    R::Future: Send + 'static,
    O: tower::Service<outbound::HttpLogical> + Send + Clone + 'static,
    O::Response: Send + 'static,
    O::Future: Send + 'static,
    O::Error: Into<Error>,
{
    type Response = Gateway<O::Response>;
    type Error = Error;
    type Future =
        Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send + 'static>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        let res = match self {
            Self::Enabled { resolve, .. } => ready!(resolve.poll_ready(cx)),
            _ => Ok(()),
        };
        Poll::Ready(res.map_err(Into::into))
    }

    fn call(&mut self, target: inbound::Target) -> Self::Future {
        let inbound::Target {
            dst,
            http_settings,
            tls_client_id,
            ..
        } = target;

        let source_identity = match tls_client_id {
            tls::Conditional::Some(id) => id,
            tls::Conditional::None(_) => {
                return Box::pin(future::ok(Gateway::NoIdentity));
            }
        };

        let orig_dst = match dst.into_name_addr() {
            Some(n) => n,
            None => {
                return Box::pin(future::ok(Gateway::NoAuthority));
            }
        };

        match self {
            Self::NoSuffixes => Box::pin(future::ok(Gateway::BadDomain(orig_dst.name().clone()))),
            Self::NoIdentity => Box::pin(future::ok(Gateway::NoIdentity)),
            Self::Enabled {
                ref mut resolve,
                outbound,
                suffixes,
                local_identity,
            } => {
                let mut outbound = outbound.clone();
                let local_identity = local_identity.clone();
                let suffixes = suffixes.clone();
                let resolve = resolve.call(orig_dst.name().clone());
                Box::pin(async move {
                    // First, resolve the original name. This determines both the
                    // canonical name as well as an IP that can be used as the outbound
                    // original dst.
                    let (name, dst_ip) = resolve.await.map_err(Into::into)?;
                    tracing::debug!(%name, %dst_ip, "Resolved");
                    if !suffixes.iter().any(|s| s.contains(&name)) {
                        tracing::debug!(%name, ?suffixes, "No matches");
                        return Ok(Gateway::BadDomain(name));
                    }

                    // Create an outbound target using the resolved IP & name.
                    let dst_addr = (dst_ip, orig_dst.port()).into();
                    let dst_name = NameAddr::new(name, orig_dst.port());
                    let endpoint = outbound::HttpLogical {
                        dst: dst_name.clone().into(),
                        orig_dst: dst_addr,
                        settings: http_settings,
                        require_identity: None,
                    };

                    let svc = outbound.call(endpoint).await.map_err(Into::into)?;
                    Ok(Gateway::new(svc, source_identity, dst_name, local_identity))
                })
            }
        }
    }
}
