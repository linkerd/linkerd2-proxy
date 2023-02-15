use super::{server::Http, Gateway};
use inbound::{GatewayAddr, GatewayDomainInvalid};
use linkerd_app_core::{identity, io, profiles, proxy::http, svc, tls, transport::addrs::*, Error};
use linkerd_app_inbound as inbound;
use linkerd_app_outbound as outbound;
use std::{
    cmp::{Eq, PartialEq},
    fmt::Debug,
    hash::Hash,
};

mod gateway;
#[cfg(test)]
mod tests;

pub(crate) use self::gateway::NewHttpGateway;

/// Target for outbound HTTP gateway stacks.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Target<T = ()> {
    addr: GatewayAddr,
    target: outbound::http::Logical,
    version: http::Version,
    parent: T,
}

/// Implements `svc::router::SelectRoute` for outbound HTTP requests. An
/// `OutboundHttp` target is returned for each request using the request's HTTP
/// version.
///
/// The request's HTTP version may not match the target's original HTTP version
/// when proxies use HTTP/2 to transport HTTP/1 requests.
#[derive(Clone, Debug)]
struct ByRequestVersion<T>(Target<T>);

impl Gateway {
    /// Wrap the provided outbound HTTP stack with an HTTP server, inbound
    /// authorization, and gateway request routing.
    pub fn http<T, I, N, NSvc>(&self, inner: N) -> svc::Stack<svc::ArcNewTcp<Http<T>, I>>
    where
        // Target describing an inbound gateway connection.
        T: svc::Param<GatewayAddr>,
        T: svc::Param<OrigDstAddr>,
        T: svc::Param<Remote<ClientAddr>>,
        T: svc::Param<tls::ConditionalServerTls>,
        T: svc::Param<tls::ClientId>,
        T: svc::Param<inbound::policy::AllowPolicy>,
        T: svc::Param<profiles::LookupAddr>,
        T: Clone + Send + Sync + Unpin + 'static,
        // Server-side socket.
        I: io::AsyncRead + io::AsyncWrite + io::PeerAddr,
        I: Send + Unpin + 'static,
        // HTTP outbound stack.
        N: svc::NewService<Target, Service = NSvc>,
        N: Clone + Send + Sync + Unpin + 'static,
        NSvc: svc::Service<
            http::Request<http::BoxBody>,
            Response = http::Response<http::BoxBody>,
            Error = Error,
        >,
        NSvc: Send + Unpin + 'static,
        NSvc::Future: Send + 'static,
    {
        let http = svc::stack(inner)
            // Discard `T` and its associated client-specific metadata.
            .push_map_target(Target::discard_parent)
            // Add headers to prevent loops.
            .push(NewHttpGateway::layer(identity::LocalId(
                self.inbound.identity().name().clone(),
            )))
            .push_on_service(svc::LoadShed::layer())
            .lift_new()
            // After protocol-downgrade, we need to build an inner stack for
            // each request-level HTTP version.
            .push(svc::NewOneshotRoute::layer_via(|t: &Target<T>| {
                ByRequestVersion(t.clone())
            }))
            // Only permit gateway traffic to endpoints for which we have
            // discovery information.
            .push_filter(
                |(_, parent): (_, Http<T>)| -> Result<_, GatewayDomainInvalid> {
                    let target = {
                        let profile = svc::Param::<Option<profiles::Receiver>>::param(&parent)
                            .ok_or(GatewayDomainInvalid)?;

                        if let Some(profiles::LogicalAddr(addr)) = profile.logical_addr() {
                            outbound::http::Logical::Route(addr, profile)
                        } else if let Some((addr, metadata)) = profile.endpoint() {
                            outbound::http::Logical::Forward(Remote(ServerAddr(addr)), metadata)
                        } else {
                            return Err(GatewayDomainInvalid);
                        }
                    };

                    Ok(Target {
                        target,
                        addr: (*parent).param(),
                        version: svc::Param::param(&parent),
                        parent: (**parent).clone(),
                    })
                },
            )
            // Authorize requests to the gateway.
            .push(self.inbound.authorize_http());

        self.inbound
            .clone()
            .with_stack(http.into_inner())
            // Teminates HTTP connections.
            // XXX Sets an identity header -- this should probably not be done
            // in the gateway, though the value will be stripped by meshed
            // servers.
            .push_http_server()
            .into_stack()
    }
}

// === impl ByRequestVersion ===

impl<B, T: Clone> svc::router::SelectRoute<http::Request<B>> for ByRequestVersion<T> {
    type Key = Target<T>;
    type Error = http::version::Unsupported;

    fn select(&self, req: &http::Request<B>) -> Result<Self::Key, Self::Error> {
        let mut t = self.0.clone();
        t.version = req.version().try_into()?;
        Ok(t)
    }
}

// === impl Target ===

impl<T> Target<T> {
    fn discard_parent(self) -> Target {
        Target {
            addr: self.addr,
            target: self.target,
            version: self.version,
            parent: (),
        }
    }
}

impl<T> svc::Param<GatewayAddr> for Target<T> {
    fn param(&self) -> GatewayAddr {
        self.addr.clone()
    }
}

impl<T> svc::Param<http::Version> for Target<T> {
    fn param(&self) -> http::Version {
        self.version
    }
}

impl<T> svc::Param<tls::ClientId> for Target<T>
where
    T: svc::Param<tls::ClientId>,
{
    fn param(&self) -> tls::ClientId {
        self.parent.param()
    }
}

impl<T> svc::Param<outbound::http::Logical> for Target<T> {
    fn param(&self) -> outbound::http::Logical {
        self.target.clone()
    }
}
