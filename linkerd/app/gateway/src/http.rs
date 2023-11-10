use super::Gateway;
use inbound::{GatewayAddr, GatewayDomainInvalid};
use linkerd_app_core::{
    metrics::ServerLabel,
    profiles,
    proxy::{
        api_resolve::{ConcreteAddr, Metadata},
        core::Resolve,
        http,
    },
    svc, tls,
    transport::addrs::*,
    Error,
};
use linkerd_app_inbound as inbound;
use linkerd_app_outbound as outbound;
use std::{
    cmp::{Eq, PartialEq},
    fmt::Debug,
};
use tokio::sync::watch;

mod gateway;
#[cfg(test)]
mod tests;

pub(crate) use self::gateway::NewHttpGateway;

/// Target for outbound HTTP gateway stacks.
#[derive(Clone, Debug)]
pub struct Target<T = ()> {
    addr: GatewayAddr,
    routes: watch::Receiver<outbound::http::Routes>,
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
    /// Wrap the provided outbound HTTP client with the inbound HTTP server,
    /// inbound authorization, tagged-transport gateway routing, and the
    /// outbound router.
    pub fn http<T, R>(
        &self,
        inner: svc::ArcNewHttp<
            outbound::http::concrete::Endpoint<
                outbound::http::logical::Concrete<outbound::http::Http<Target>>,
            >,
        >,
        resolve: R,
    ) -> svc::Stack<
        svc::ArcNewService<
            T,
            impl svc::Service<
                    http::Request<http::BoxBody>,
                    Response = http::Response<http::BoxBody>,
                    Error = Error,
                    Future = impl Send,
                > + Clone,
        >,
    >
    where
        // Target describing an inbound gateway connection.
        T: svc::Param<GatewayAddr>,
        T: svc::Param<OrigDstAddr>,
        T: svc::Param<Remote<ClientAddr>>,
        T: svc::Param<ServerLabel>,
        T: svc::Param<tls::ConditionalServerTls>,
        T: svc::Param<tls::ClientId>,
        T: svc::Param<inbound::policy::AllowPolicy>,
        T: svc::Param<Option<watch::Receiver<profiles::Profile>>>,
        T: svc::Param<http::Version>,
        T: svc::Param<http::normalize_uri::DefaultAuthority>,
        T: Clone + Send + Sync + Unpin + 'static,
        // Endpoint resolution.
        R: Resolve<ConcreteAddr, Endpoint = Metadata, Error = Error>,
    {
        let http = self
            .outbound
            .clone()
            .with_stack(inner)
            .push_http_cached(resolve)
            .into_stack()
            // Discard `T` and its associated client-specific metadata.
            .push_map_target(Target::discard_parent)
            .push(svc::ArcNewService::layer())
            // Add headers to prevent loops.
            .push(NewHttpGateway::layer(
                self.inbound.identity().local_id().clone(),
            ))
            .push_on_service(svc::LoadShed::layer())
            .lift_new()
            .push(svc::ArcNewService::layer())
            // After protocol-downgrade, we need to build an inner stack for
            // each request-level HTTP version.
            .push(svc::NewOneshotRoute::layer_via(|t: &Target<T>| {
                ByRequestVersion(t.clone())
            }))
            // Only permit gateway traffic to endpoints for which we have
            // discovery information.
            .push_filter(|(_, parent): (_, T)| -> Result<_, GatewayDomainInvalid> {
                let routes = {
                    let mut profile =
                        svc::Param::<Option<watch::Receiver<profiles::Profile>>>::param(&parent)
                            .ok_or(GatewayDomainInvalid)?;
                    let init =
                        mk_routes(&profile.borrow_and_update()).ok_or(GatewayDomainInvalid)?;
                    outbound::http::spawn_routes(profile, init, mk_routes)
                };

                Ok(Target {
                    routes,
                    addr: parent.param(),
                    version: parent.param(),
                    parent,
                })
            })
            .push(svc::ArcNewService::layer())
            // Authorize requests to the gateway.
            .push(self.inbound.authorize_http())
            .arc_box_new_clone_http();

        self.inbound
            .clone()
            .with_stack(http.into_inner())
            // Teminates HTTP connections.
            // XXX Sets an identity header -- this should probably not be done
            // in the gateway, though the value will be stripped by meshed
            // servers.
            .push_http_server()
            .into_stack()
            .arc_box_new_clone_http()
    }
}

fn mk_routes(profile: &profiles::Profile) -> Option<outbound::http::Routes> {
    if let Some(addr) = profile.addr.clone() {
        return Some(outbound::http::Routes::Profile(
            outbound::http::profile::Routes {
                addr,
                routes: profile.http_routes.clone(),
                targets: profile.targets.clone(),
            },
        ));
    }

    if let Some((addr, metadata)) = profile.endpoint.clone() {
        return Some(outbound::http::Routes::Endpoint(
            Remote(ServerAddr(addr)),
            metadata,
        ));
    }

    None
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
            routes: self.routes,
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

impl<T> svc::Param<watch::Receiver<outbound::http::Routes>> for Target<T> {
    fn param(&self) -> watch::Receiver<outbound::http::Routes> {
        self.routes.clone()
    }
}

impl<T> PartialEq for Target<T> {
    fn eq(&self, other: &Self) -> bool {
        self.addr == other.addr && self.version == other.version
    }
}

impl<T> Eq for Target<T> {}

impl<T: std::hash::Hash> std::hash::Hash for Target<T> {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.addr.hash(state);
        self.version.hash(state);
        self.parent.hash(state);
    }
}
