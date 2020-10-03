use super::gateway::Gateway;
use futures::future;
use linkerd2_app_core::{dns, profiles, proxy::identity, transport::tls, Addr, Error, NameAddr};
use linkerd2_app_inbound::endpoint as inbound;
use linkerd2_app_outbound::endpoint as outbound;
use std::{
    future::Future,
    net::SocketAddr,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};

#[derive(Clone, Debug)]
pub(crate) enum MakeGateway<P, O> {
    NoIdentity,
    NoSuffixes,
    Enabled {
        profiles: P,
        outbound: O,
        suffixes: Arc<Vec<dns::Suffix>>,
        local_identity: identity::Name,
        default_addr: SocketAddr,
    },
}

#[derive(Clone, Debug)]
pub struct ProfileAddr(NameAddr);

impl Into<Addr> for &'_ ProfileAddr {
    fn into(self) -> Addr {
        self.0.clone().into()
    }
}

impl<P, O> MakeGateway<P, O> {
    pub fn new(
        profiles: P,
        outbound: O,
        default_addr: SocketAddr,
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
        Self::Enabled {
            profiles,
            outbound,
            local_identity,
            suffixes,
            default_addr,
        }
    }
}

impl<P, O> tower::Service<inbound::Logical> for MakeGateway<P, O>
where
    P: profiles::GetProfile<ProfileAddr>,
    P::Future: Send + 'static,
    O: tower::Service<outbound::HttpLogical> + Send + Clone + 'static,
    O::Response: Send + 'static,
    O::Future: Send + 'static,
    O::Error: Into<Error>,
{
    type Response = Gateway<O::Response>;
    type Error = Error;
    type Future =
        Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send + 'static>>;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, target: inbound::Logical) -> Self::Future {
        let inbound::Logical {
            profiles,
            target: inbound::Target {
                dst,
                http_version,
                tls_client_id,
                ..
            },
        } = target;

        let source_identity = match tls_client_id {
            tls::Conditional::Some(id) => id,
            tls::Conditional::None(_) => {
                return Box::pin(future::ok(Gateway::NoIdentity));
            }
        };

        let dst = match dst.into_name_addr() {
            Some(n) => n,
            None => {
                return Box::pin(future::ok(Gateway::NoAuthority));
            }
        };

        match self {
            Self::NoSuffixes => Box::pin(future::ok(Gateway::BadDomain(dst.name().clone()))),
            Self::NoIdentity => Box::pin(future::ok(Gateway::NoIdentity)),
            Self::Enabled {
                ref mut profiles,
                outbound,
                suffixes,
                local_identity,
                default_addr,
            } => {
                if !suffixes.iter().any(|s| s.contains(dst.name())) {
                    tracing::debug!(name=%dst.name(), ?suffixes, "No matches");
                    return Box::pin(future::ok(Gateway::BadDomain(dst.name().clone())));
                }

                let mut outbound = outbound.clone();
                let local_identity = local_identity.clone();
                let default_addr = *default_addr;

                let resolve = profiles.get_profile(ProfileAddr(dst.clone()));
                Box::pin(async move {
                    let profile = resolve.await.map_err(Into::into)?;
                    if profile.is_none() {
                        tracing::debug!("Profile lookup rejected");
                        return Ok(Gateway::BadDomain(dst.name().clone()));
                    }
                    tracing::debug!("Resolved profile");

                    // Create an outbound target using the resolved IP & name.
                    let endpoint = outbound::HttpLogical {
                        orig_dst: default_addr,
                        version: http_version,
                        profile,
                    };

                    let svc = outbound.call(endpoint).await.map_err(Into::into)?;
                    Ok(Gateway::new(svc, source_identity, dst, local_identity))
                })
            }
        }
    }
}
